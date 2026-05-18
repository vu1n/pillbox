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
}
