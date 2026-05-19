//! OpenAI API-key provider.
//!
//! Intercepts `api.openai.com` and swaps a pillbox-minted stub key for
//! the real one in the `Authorization: Bearer <stub>` header.
//!
//! This is **API-key only** — codex's ChatGPT OAuth lives on
//! `chatgpt.com` / `auth.openai.com` and is handled by `codex.rs`. They
//! never collide because the hosts differ.
//!
//! The stub is minted by `Server::lease_api_key` (not `provision`) — this
//! provider doesn't own a credentials file; the secret value lives in
//! a `--with NAME=ENV_VAR` env var inside the guest.

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

const PROVIDER_ID: &str = "openai-api-key";
const API_HOST: &str = "api.openai.com";

pub(crate) struct OpenAiApiKeyProvider;

#[async_trait]
impl VaultProvider for OpenAiApiKeyProvider {
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
        match swap_bearer_style(&auth_value, "Bearer", server) {
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
        // API keys never rotate inline. No tear-down work needed.
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::providers::{Registry, SandboxData, API_KEY_PROVIDER_ID};

    #[test]
    fn intercept_only_api_openai() {
        let p = OpenAiApiKeyProvider;
        assert!(p.intercept("api.openai.com"));
        // codex's hosts must NOT be intercepted by this provider.
        assert!(!p.intercept("chatgpt.com"));
        assert!(!p.intercept("chat.openai.com"));
        assert!(!p.intercept("auth.openai.com"));
        assert!(!p.intercept("api.anthropic.com"));
    }

    #[test]
    fn provision_is_an_error_for_api_key_provider() {
        let mut r = Registry::new();
        let err = OpenAiApiKeyProvider
            .provision("sbx-x", &serde_json::json!({}), &mut r)
            .unwrap_err();
        assert!(err.contains("lease_api_key"), "got: {err}");
    }

    #[test]
    fn api_key_lookup_returns_real_via_registry() {
        let mut r = Registry::new();
        let stub = "sk-stubbedvalue123";
        r.insert(
            "sbx-1".into(),
            SandboxData {
                provider_id: API_KEY_PROVIDER_ID,
                real: serde_json::json!({"name": "OPENAI_API_KEY", "value": "sk-real-xyz"}),
                stubs: vec![stub.into()],
            },
        );
        assert_eq!(r.api_key_real_for_stub(stub), Some("sk-real-xyz"));
        assert!(r.api_key_real_for_stub("sk-unknown").is_none());
    }

    // ── End-to-end Request/Response integration tests ────────────────

    use crate::vault::known_secrets::HeaderScheme;
    use crate::vault::providers::test_support::{
        body_bytes, build_request, cleanup, expect_request, fresh_server,
    };
    use crate::vault::providers::PendingFlow;
    use hudsucker::{hyper::Request as HReq, Body};

    #[tokio::test]
    async fn bearer_request_swaps_stub_to_real_api_key() {
        let (server, dir) = fresh_server().await;
        let (_lease, stub) = server
            .lease_api_key_for_test(
                "OPENAI_API_KEY",
                "sk-real-openai-key",
                "api.openai.com",
                HeaderScheme::AuthorizationBearer,
                "sk-",
            )
            .expect("lease api key");

        let req = build_request(
            "POST",
            "https://api.openai.com/v1/chat/completions",
            Body::empty(),
        );
        let req = {
            let (mut parts, body) = req.into_parts();
            parts
                .headers
                .insert("authorization", format!("Bearer {stub}").parse().unwrap());
            HReq::from_parts(parts, body)
        };

        let mut pending: Option<PendingFlow> = None;
        let out = OpenAiApiKeyProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "openai bearer swap");
        let auth = out_req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer sk-real-openai-key");
        assert!(pending.is_none());

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn bearer_unknown_stub_passes_through() {
        // OpenAI provider uses `swap_bearer_style`, which returns
        // `PassThrough` for unrecognised stubs (preserves the
        // `--with OPENAI_API_KEY` no-vault path).
        let (server, dir) = fresh_server().await;
        let req = build_request(
            "POST",
            "https://api.openai.com/v1/chat/completions",
            Body::empty(),
        );
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                "authorization",
                "Bearer sk-unknown-passthrough".parse().unwrap(),
            );
            HReq::from_parts(parts, body)
        };

        let mut pending = None;
        let out = OpenAiApiKeyProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "openai pass-through");
        assert_eq!(
            out_req
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer sk-unknown-passthrough"
        );

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn no_auth_header_passes_through() {
        let (server, dir) = fresh_server().await;
        let req = build_request("GET", "https://api.openai.com/v1/models", Body::empty());
        let mut pending = None;
        let out = OpenAiApiKeyProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "openai no-auth");
        assert!(out_req.headers().get("authorization").is_none());
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn anthropic_stub_routed_to_openai_handler_is_no_op() {
        // Cross-provider routing sanity: an anthropic-OAuth bearer
        // (sk-ant-oat01-…) reaching the openai handler should NOT
        // resolve via openai (the registry entry is OAuth-shaped, not
        // API-key-shaped) — provider must pass through unchanged.
        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease(
                "claude",
                "sbx-cross",
                crate::vault::providers::test_support::sample_anthropic_real(),
            )
            .expect("lease");
        let stub_access = {
            let registry = server.registry_lock_for_test();
            registry
                .stubs_for("sbx-cross")
                .unwrap()
                .iter()
                .find(|s| s.starts_with("sk-ant-oat01-"))
                .cloned()
                .unwrap()
        };

        let req = build_request(
            "POST",
            "https://api.openai.com/v1/chat/completions",
            Body::empty(),
        );
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                "authorization",
                format!("Bearer {stub_access}").parse().unwrap(),
            );
            HReq::from_parts(parts, body)
        };

        let mut pending = None;
        let out = OpenAiApiKeyProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "cross-provider no-op");
        // Bearer header untouched.
        assert_eq!(
            out_req
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {stub_access}")
        );

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn handle_response_is_pass_through() {
        let (server, dir) = fresh_server().await;
        let res = hudsucker::hyper::Response::builder()
            .status(200)
            .body(Body::from("hello"))
            .unwrap();
        let mut pending = None;
        let out = OpenAiApiKeyProvider
            .handle_response(res, server.inner_for_test(), &mut pending)
            .await;
        let bytes = body_bytes(out.into_body()).await;
        assert_eq!(bytes, b"hello");
        cleanup(server, dir);
    }
}
