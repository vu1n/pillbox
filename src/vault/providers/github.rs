//! GitHub PAT (personal access token) provider.
//!
//! Intercepts `api.github.com` and swaps a pillbox-minted stub for the
//! real PAT in the `Authorization` header. GitHub accepts two schemes
//! interchangeably:
//!  - `Authorization: Bearer <token>` — modern fine-grained PATs, gh CLI
//!  - `Authorization: token <token>` — older clients, octokit defaults
//!
//! Both are handled.
//!
//! Like the OpenAI provider, the value travels via `--with NAME=ENV_VAR`
//! in the guest env — no credentials file on disk.

use std::path::Path;

use async_trait::async_trait;
use hudsucker::{
    hyper::{header::AUTHORIZATION, Request, Response},
    Body, RequestOrResponse,
};

use super::{
    host_from_uri, provision_is_api_key_only, swap_bearer_style, unauthorized, ApiKeySwap,
    PendingFlow, Registry, VaultProvider, API_KEY_UNUSED_CREDS_PATH,
};
use crate::vault::server::ServerInner;

const PROVIDER_ID: &str = "github-pat";
const API_HOST: &str = "api.github.com";

pub(crate) struct GithubProvider;

#[async_trait]
impl VaultProvider for GithubProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn intercept(&self, host: &str) -> bool {
        host == API_HOST
    }

    fn hosts(&self) -> &'static [&'static str] {
        &[API_HOST]
    }

    fn creds_path(&self) -> &'static Path {
        Path::new(API_KEY_UNUSED_CREDS_PATH)
    }

    fn provision(
        &self,
        _sandbox_id: &str,
        _real: &serde_json::Value,
        _registry: &mut Registry,
    ) -> Result<String, String> {
        provision_is_api_key_only(PROVIDER_ID)
    }

    async fn handle_request(
        &self,
        req: Request<Body>,
        server: &ServerInner,
        _pending: &mut Option<PendingFlow>,
    ) -> RequestOrResponse {
        let host = host_from_uri(&req).unwrap_or_default();
        if host != API_HOST {
            return req.into();
        }
        let (mut parts, body) = req.into_parts();
        let Some(auth_value) = parts.headers.get(AUTHORIZATION).cloned() else {
            return Request::from_parts(parts, body).into();
        };

        // Try Bearer first, then legacy `token`. Preserve the inbound
        // scheme so clients that distinguish (none we know of) don't
        // see a surprise.
        let auth_str = match auth_value.to_str() {
            Ok(s) => s,
            Err(_) => return unauthorized("non-utf8 authorization").into(),
        };
        let scheme = if auth_str.starts_with("Bearer ") {
            "Bearer"
        } else if auth_str.starts_with("token ") {
            "token"
        } else {
            // Unknown scheme — pass through. GitHub will reject it on its end
            // if it's bogus.
            return Request::from_parts(parts, body).into();
        };

        match swap_bearer_style(&auth_value, scheme, server) {
            ApiKeySwap::Swapped(hv) => {
                parts.headers.insert(AUTHORIZATION, hv);
                Request::from_parts(parts, body).into()
            }
            ApiKeySwap::PassThrough => Request::from_parts(parts, body).into(),
            ApiKeySwap::Unauthorized(detail) => unauthorized(detail).into(),
        }
    }

    async fn handle_response(
        &self,
        res: Response<Body>,
        _server: &ServerInner,
        _pending: &mut Option<PendingFlow>,
    ) -> Response<Body> {
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::providers::{Registry, SandboxData, API_KEY_PROVIDER_ID};

    #[test]
    fn intercept_matches_api_github_only() {
        let p = GithubProvider;
        assert!(p.intercept("api.github.com"));
        assert!(!p.intercept("github.com"));
        assert!(!p.intercept("raw.githubusercontent.com"));
    }

    #[test]
    fn provision_errors_for_api_key_provider() {
        let mut r = Registry::new();
        let err = GithubProvider
            .provision("sbx-x", &serde_json::json!({}), &mut r)
            .unwrap_err();
        assert!(err.contains("lease_api_key"), "got: {err}");
    }

    #[test]
    fn api_key_lookup_returns_real_via_registry() {
        let mut r = Registry::new();
        let stub = "ghp_stub123";
        r.insert(
            "sbx-1".into(),
            SandboxData {
                provider_id: API_KEY_PROVIDER_ID,
                real: serde_json::json!({"name": "GITHUB_TOKEN", "value": "ghp_realtoken"}),
                stubs: vec![stub.into()],
            },
        );
        assert_eq!(r.api_key_real_for_stub(stub), Some("ghp_realtoken"));
    }

    // ── End-to-end Request/Response integration tests ────────────────

    use crate::vault::known_secrets::HeaderScheme;
    use crate::vault::providers::test_support::{
        build_request, cleanup, expect_request, fresh_server,
    };
    use crate::vault::providers::PendingFlow;
    use hudsucker::{hyper::Request as HReq, Body};

    fn lease_github_pat(
        server: &crate::vault::server::Server,
        real: &str,
    ) -> (crate::vault::lease::SandboxLease, String) {
        server
            .lease_api_key_for_test(
                "GITHUB_TOKEN",
                real,
                "api.github.com",
                HeaderScheme::AuthorizationBearer,
                "ghp_",
            )
            .expect("lease github pat")
    }

    #[tokio::test]
    async fn bearer_request_swaps_stub_to_real_pat() {
        let (server, dir) = fresh_server().await;
        let (_lease, stub) = lease_github_pat(&server, "ghp_realpat_value");

        let req = build_request("GET", "https://api.github.com/user", Body::empty());
        let req = {
            let (mut parts, body) = req.into_parts();
            parts
                .headers
                .insert("authorization", format!("Bearer {stub}").parse().unwrap());
            HReq::from_parts(parts, body)
        };

        let mut pending: Option<PendingFlow> = None;
        let out = GithubProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "github bearer swap");
        assert_eq!(
            out_req
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer ghp_realpat_value"
        );

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn token_scheme_legacy_request_swaps_stub_to_real_pat() {
        // octokit defaults to `Authorization: token <pat>`. The provider
        // must preserve that scheme on the way out.
        let (server, dir) = fresh_server().await;
        let (_lease, stub) = lease_github_pat(&server, "ghp_legacytoken_value");

        let req = build_request("GET", "https://api.github.com/repos/me/foo", Body::empty());
        let req = {
            let (mut parts, body) = req.into_parts();
            parts
                .headers
                .insert("authorization", format!("token {stub}").parse().unwrap());
            HReq::from_parts(parts, body)
        };

        let mut pending = None;
        let out = GithubProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "github token swap");
        assert_eq!(
            out_req
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "token ghp_legacytoken_value"
        );

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn unknown_scheme_passes_through() {
        // Anything that isn't `Bearer ` / `token ` is left alone for
        // GitHub to reject on its end.
        let (server, dir) = fresh_server().await;
        let req = build_request("GET", "https://api.github.com/user", Body::empty());
        let req = {
            let (mut parts, body) = req.into_parts();
            parts
                .headers
                .insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
            HReq::from_parts(parts, body)
        };

        let mut pending = None;
        let out = GithubProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "github unknown scheme pass-through");
        assert_eq!(
            out_req
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcjpwYXNz"
        );

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn unknown_stub_passes_through() {
        // Bearer-style with a stub the registry doesn't recognise =>
        // `swap_bearer_style` returns PassThrough.
        let (server, dir) = fresh_server().await;
        let req = build_request("GET", "https://api.github.com/user", Body::empty());
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                "authorization",
                "Bearer ghp_not_in_registry".parse().unwrap(),
            );
            HReq::from_parts(parts, body)
        };
        let mut pending = None;
        let out = GithubProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "github unknown stub");
        assert_eq!(
            out_req
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer ghp_not_in_registry"
        );

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn no_auth_header_passes_through() {
        let (server, dir) = fresh_server().await;
        let req = build_request("GET", "https://api.github.com/", Body::empty());
        let mut pending = None;
        let out = GithubProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "github no-auth");
        assert!(out_req.headers().get("authorization").is_none());
        cleanup(server, dir);
    }
}
