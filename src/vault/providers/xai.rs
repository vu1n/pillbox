//! xAI API-key provider.
//!
//! Intercepts `api.x.ai` and swaps a pillbox-minted stub key for the real
//! `XAI_API_KEY` in the `Authorization: Bearer <stub>` header. Grok Build's
//! headless CLI sends that header. The text/2 microVM applies the same swap
//! inside the VMM child; this provider is what `pillbox secret add
//! XAI_API_KEY --vault` leases against.

use std::path::Path;

use async_trait::async_trait;
use hudsucker::{
    hyper::{header::AUTHORIZATION, Request},
    Body, RequestOrResponse,
};

use super::{
    host_from_uri, provision_is_api_key_only, swap_bearer_style, unauthorized, ApiKeySwap,
    Registry, VaultProvider, API_KEY_UNUSED_CREDS_PATH,
};
use crate::vault::server::ServerInner;

const PROVIDER_ID: &str = "xai-api-key";
const API_HOST: &str = "api.x.ai";

pub(crate) struct XaiApiKeyProvider;

#[async_trait]
impl VaultProvider for XaiApiKeyProvider {
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

    async fn handle_request(&self, req: Request<Body>, server: &ServerInner) -> RequestOrResponse {
        let host = host_from_uri(&req).unwrap_or_default();
        if host != API_HOST {
            return req.into();
        }
        let (mut parts, body) = req.into_parts();
        let Some(auth_value) = parts.headers.get(AUTHORIZATION).cloned() else {
            return Request::from_parts(parts, body).into();
        };
        match swap_bearer_style(&auth_value, "Bearer", server, &host) {
            ApiKeySwap::Swapped(hv) => {
                parts.headers.insert(AUTHORIZATION, hv);
                Request::from_parts(parts, body).into()
            }
            ApiKeySwap::PassThrough => Request::from_parts(parts, body).into(),
            ApiKeySwap::Unauthorized(detail) => unauthorized(detail).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::known_secrets::HeaderScheme;
    use crate::vault::providers::test_support::{
        build_request, cleanup, expect_request, fresh_server,
    };
    use hudsucker::{hyper::Request as HReq, Body};

    #[test]
    fn intercept_only_api_xai() {
        let provider = XaiApiKeyProvider;
        assert!(provider.intercept("api.x.ai"));
        assert!(!provider.intercept("api.openai.com"));
        assert!(!provider.intercept("chatgpt.com"));
    }

    #[test]
    fn provision_is_an_error_for_api_key_provider() {
        let mut registry = Registry::new();
        let err = XaiApiKeyProvider
            .provision("sbx-x", &serde_json::json!({}), &mut registry)
            .unwrap_err();
        assert!(err.contains("lease_api_key"), "got: {err}");
    }

    #[tokio::test]
    async fn bearer_request_swaps_stub_to_real_api_key() {
        let (server, dir) = fresh_server().await;
        let (_lease, stub) = server
            .lease_api_key_for_test(
                "XAI_API_KEY",
                "xai-real-key",
                "api.x.ai",
                HeaderScheme::AuthorizationBearer,
                "xai-",
            )
            .expect("lease api key");

        let req = build_request("POST", "https://api.x.ai/v1/responses", Body::empty());
        let req = {
            let (mut parts, body) = req.into_parts();
            parts
                .headers
                .insert("authorization", format!("Bearer {stub}").parse().unwrap());
            HReq::from_parts(parts, body)
        };

        let out = XaiApiKeyProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "xai bearer swap");
        let auth = out_req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer xai-real-key");

        drop(_lease);
        cleanup(server, dir);
    }
}
