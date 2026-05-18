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
}
