//! Codex provider — OpenAI ChatGPT-mode OAuth.
//!
//! v0.5 scope: ChatGPT mode only. The codex `auth.json` schema is:
//!
//! ```jsonc
//! {
//!   "auth_mode": "ChatGPT",
//!   "tokens": {
//!     "id_token": "<JWT>",        // identity claims; copied verbatim into the stub
//!     "access_token": "<JWT>",    // stubbed; released only to API hosts
//!     "refresh_token": "<opaque>",// shape-valid stub; never released
//!     "account_id": "<id>"        // copied verbatim
//!   },
//!   "last_refresh": "<ISO ts>",
//!   "agent_identity": null
//! }
//! ```
//!
//! ApiKey-mode auth.json (with `OPENAI_API_KEY` set instead of `tokens`)
//! is rejected — that path needs the API-key vault track and isn't ready
//! yet (pillbox task #26).
//!
//! Intercepted hosts:
//!  - `chatgpt.com` (exact + any subdomain) — bearer-token swap on every
//!    request.
//!  - `chat.openai.com` — same.
//!  - `auth.openai.com` — `/oauth/token` is intercepted and rejected locally
//!    (verified against `codex-rs/login/src/auth/manager.rs`:
//!    `const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token"`).
//!
//! `api.openai.com` is **not** intercepted in v0.5 — that's the API-key
//! path, deferred until the ApiKey vault track lands.

use std::path::Path;

use async_trait::async_trait;
use hudsucker::{
    hyper::{
        header::{HeaderValue, AUTHORIZATION},
        Request,
    },
    Body, RequestOrResponse,
};

use super::{
    host_from_uri, mint_stub, oauth_rotation_forbidden, unauthorized, HostProxyOAuthStub,
    OAuthCodec, OAuthRefreshRequest, Registry, SandboxData, VaultProvider,
};
#[cfg(feature = "libkrun")]
use super::{LibkrunOAuthStub, OAuthRelease};
use crate::vault::server::ServerInner;
use crate::vault::token_store::RefreshDecider;

const PROVIDER_ID: &str = "codex";

const CHATGPT_HOST: &str = "chatgpt.com";
const CHATGPT_HOST_DOT: &str = ".chatgpt.com";
const CHAT_OPENAI_HOST: &str = "chat.openai.com";
const AUTH_OPENAI_HOST: &str = "auth.openai.com";
const OAUTH_TOKEN_PATH_SUFFIX: &str = "/oauth/token";
const CREDS_PATH: &str = ".codex/auth.json";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_ENDPOINT: &str = "https://auth.openai.com/oauth/token";

// Codex doesn't ship a public stub prefix convention (tokens are opaque
// JWTs / random strings). The `pb-codex-` family makes any logged stub
// visibly a pillbox artifact rather than a real OpenAI token.
pub(crate) const STUB_ACCESS_PREFIX: &str = "pb-codex-oat-";
pub(crate) const STUB_REFRESH_PREFIX: &str = "pb-codex-ort-";

pub(crate) struct CodexProvider;

#[async_trait]
impl VaultProvider for CodexProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn intercept(&self, host: &str) -> bool {
        host == CHATGPT_HOST
            || host.ends_with(CHATGPT_HOST_DOT)
            || host == CHAT_OPENAI_HOST
            || host == AUTH_OPENAI_HOST
    }

    /// Concrete hosts for the DNS-fence allowlist. The `*.chatgpt.com` wildcard
    /// `intercept` also matches isn't enumerable for an exact-match DNS allowlist
    /// — a known gap for codex subdomains (claude/the api hosts are exact).
    fn hosts(&self) -> &'static [&'static str] {
        &[CHATGPT_HOST, CHAT_OPENAI_HOST, AUTH_OPENAI_HOST]
    }

    fn creds_path(&self) -> &'static Path {
        Path::new(CREDS_PATH)
    }

    fn oauth_codec(&self) -> Option<&dyn OAuthCodec> {
        Some(self)
    }

    fn is_oauth_token_endpoint(&self, host: &str, path: &str) -> bool {
        host == AUTH_OPENAI_HOST && path.ends_with(OAUTH_TOKEN_PATH_SUFFIX)
    }

    fn provision(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
        registry: &mut Registry,
    ) -> Result<String, String> {
        let stub = self.host_proxy_stub(sandbox_id, real)?;
        let stub_json = serde_json::to_string_pretty(&stub.credentials)
            .map_err(|error| format!("serialize stub auth.json: {error}"))?;

        registry.insert(
            sandbox_id.to_string(),
            SandboxData {
                provider_id: PROVIDER_ID,
                real: real.clone(),
                stubs: stub.stubs,
            },
        );

        Ok(stub_json)
    }

    async fn handle_request(&self, req: Request<Body>, server: &ServerInner) -> RequestOrResponse {
        let host = host_from_uri(&req).unwrap_or_default();
        if self.is_oauth_token_endpoint(&host, req.uri().path()) {
            return oauth_rotation_forbidden().into();
        }
        // chatgpt.com / chat.openai.com / subdomains all use a Bearer
        // access token in the Authorization header for codex-cli's
        // backend calls.
        handle_bearer_request(req, server).await
    }
}

impl RefreshDecider for CodexProvider {
    fn needs_refresh(&self, creds: &serde_json::Value) -> bool {
        self.expiry_ms(creds)
            .map(crate::vault::refresh::is_expired)
            .unwrap_or(true)
    }

    fn refresh_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/tokens/refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn access_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/tokens/access_token")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn access_usable(&self, creds: &serde_json::Value) -> bool {
        self.access_token(creds).is_some()
            && self
                .expiry_ms(creds)
                .is_some_and(|expiry| expiry > crate::vault::refresh::unix_now_ms())
    }
}

impl OAuthCodec for CodexProvider {
    fn refresh_request(&self, refresh_token: &str) -> OAuthRefreshRequest {
        OAuthRefreshRequest {
            endpoint: OAUTH_ENDPOINT,
            body: serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": OAUTH_CLIENT_ID,
            }),
        }
    }

    fn apply_refresh_response(
        &self,
        real: &mut serde_json::Value,
        response: &serde_json::Value,
        old_refresh: &str,
        now_ms: u64,
    ) -> anyhow::Result<()> {
        let access = response
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime(
                    "vault",
                    "refresh response missing access_token",
                )
            })?;
        let refresh = response
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or(old_refresh);
        let tokens = real
            .get_mut("tokens")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime("vault", "codex tokens block disappeared")
            })?;
        tokens.insert("access_token".into(), access.into());
        tokens.insert("refresh_token".into(), refresh.into());
        if let Some(id_token) = response.get("id_token").and_then(|v| v.as_str()) {
            tokens.insert("id_token".into(), id_token.into());
        }
        if self.expiry_ms(real).is_none() {
            return Err(crate::errors::PillboxError::runtime(
                "vault",
                "codex refresh response access_token has no usable JWT expiry",
            )
            .into());
        }
        let refreshed_at = time::OffsetDateTime::from_unix_timestamp_nanos(
            i128::from(now_ms).saturating_mul(1_000_000),
        )
        .map_err(|_| {
            crate::errors::PillboxError::runtime("vault", "refresh timestamp out of range")
        })?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| crate::errors::PillboxError::runtime("vault", "format refresh timestamp"))?;
        real.as_object_mut()
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime("vault", "codex auth.json is not an object")
            })?
            .insert("last_refresh".into(), refreshed_at.into());
        Ok(())
    }

    fn expiry_ms(&self, real: &serde_json::Value) -> Option<u64> {
        use base64::Engine as _;
        let token = real.pointer("/tokens/access_token")?.as_str()?;
        let payload = token.split('.').nth(1)?;
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        Some(claims.get("exp")?.as_u64()?.saturating_mul(1000))
    }

    fn host_proxy_stub(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
    ) -> Result<HostProxyOAuthStub, String> {
        let obj = real
            .as_object()
            .ok_or_else(|| "codex auth.json must be a JSON object".to_string())?;
        let tokens = match obj.get("tokens") {
            Some(serde_json::Value::Object(map)) => map,
            Some(serde_json::Value::Null) | None => {
                return Err("codex auth.json has no `tokens` block (ApiKey mode). \
                     v0.5 vault supports ChatGPT mode only. \
                     The API-key path lands with the API-key vault track \
                     (pillbox task #26)."
                    .into());
            }
            Some(_) => return Err("codex auth.json `tokens` is not an object".into()),
        };
        let access = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "codex auth.json: tokens.access_token missing".to_string())?;
        tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "codex auth.json: tokens.refresh_token missing".to_string())?;
        if access.split('.').count() != 3 {
            return Err(
                "codex auth.json: tokens.access_token is not a JWT (expected 3 dot-separated parts)"
                    .into(),
            );
        }
        let access = mint_stub(STUB_ACCESS_PREFIX, sandbox_id);
        let refresh = mint_stub(STUB_REFRESH_PREFIX, sandbox_id);
        let mut credentials = real.clone();
        let tokens = credentials
            .get_mut("tokens")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| "tokens block missing during stub build".to_string())?;
        tokens.insert("access_token".into(), access.clone().into());
        tokens.insert("refresh_token".into(), refresh.clone().into());
        Ok(HostProxyOAuthStub {
            credentials,
            stubs: vec![access, refresh],
        })
    }

    #[cfg(feature = "libkrun")]
    fn libkrun_stub(&self, real: &mut serde_json::Value) -> LibkrunOAuthStub {
        let Some(tokens) = real.get_mut("tokens").and_then(|v| v.as_object_mut()) else {
            return LibkrunOAuthStub {
                access_stub: None,
                releases: Vec::new(),
            };
        };
        let mut releases = Vec::new();
        let access_stub = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .map(|real| {
                let stub = mint_libkrun_access_stub();
                tokens.insert("access_token".into(), stub.clone().into());
                releases.push(OAuthRelease {
                    stub: stub.clone(),
                    real,
                });
                stub
            });
        if tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
        {
            tokens.insert(
                "refresh_token".into(),
                format!(
                    "{STUB_REFRESH_PREFIX}pllbxstub{}",
                    uuid::Uuid::now_v7().simple()
                )
                .into(),
            );
        }
        LibkrunOAuthStub {
            access_stub,
            releases,
        }
    }
}

#[cfg(feature = "libkrun")]
fn mint_libkrun_access_stub() -> String {
    use base64::Engine as _;
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let payload = serde_json::to_vec(&serde_json::json!({
        "exp": crate::vault::STUB_FAR_FUTURE_EXPIRES_AT_MS / 1000,
    }))
    .expect("static codex stub claims serialize");
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    let signature = format!(
        "{STUB_ACCESS_PREFIX}pllbxstub{}",
        uuid::Uuid::now_v7().simple()
    );
    format!("{header}.{payload}.{signature}")
}

/// Swap stub bearer access_token → real bearer access_token on the way
/// out to chatgpt.com / chat.openai.com.
async fn handle_bearer_request(req: Request<Body>, server: &ServerInner) -> RequestOrResponse {
    let (mut parts, body) = req.into_parts();

    let Some(auth_value) = parts.headers.get(AUTHORIZATION).cloned() else {
        // No Authorization header — let upstream return its own error.
        return Request::from_parts(parts, body).into();
    };
    let Ok(auth_str) = auth_value.to_str() else {
        return unauthorized("non-utf8 authorization").into();
    };
    let Some(stub) = auth_str.strip_prefix("Bearer ") else {
        return unauthorized("non-bearer authorization").into();
    };

    // Only swap stubs minted by this provider. If the stub doesn't carry
    // the codex prefix, it's not ours — pass it through unchanged so we
    // don't accidentally 401 a future provider's traffic if hosts ever
    // overlap.
    if !stub.starts_with(STUB_ACCESS_PREFIX) {
        return Request::from_parts(parts, body).into();
    }

    let real_access = {
        let registry = server.registry_lock();
        let sandbox_id = match registry.sandbox_for_stub(stub) {
            Some(s) => s.to_string(),
            None => return unauthorized("unknown codex stub access token").into(),
        };
        registry
            .real(&sandbox_id)
            .and_then(|v| v.pointer("/tokens/access_token"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };

    if let Some(real) = real_access {
        let new_value = format!("Bearer {real}");
        match HeaderValue::from_str(&new_value) {
            Ok(hv) => {
                parts.headers.insert(AUTHORIZATION, hv);
            }
            Err(error) => {
                eprintln!("pillbox: vault: invalid real codex access token header: {error}");
                return unauthorized("invalid real token").into();
            }
        }
    }

    Request::from_parts(parts, body).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::providers::test_support::FAKE_JWT;

    fn sample_chatgpt_real() -> serde_json::Value {
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "id_token": FAKE_JWT,
                "access_token": FAKE_JWT,
                "refresh_token": "rt_opaque_real_xyz",
                "account_id": "acct_abc"
            },
            "last_refresh": "2026-05-18T00:00:00Z",
            "agent_identity": serde_json::Value::Null,
            // Pretend an unknown field codex might add in the future.
            "future_feature": "preserve_me"
        })
    }

    fn sample_apikey_real() -> serde_json::Value {
        serde_json::json!({
            "OPENAI_API_KEY": "sk-real-api-key",
            "tokens": serde_json::Value::Null
        })
    }

    #[test]
    fn provision_chatgpt_mode_mints_stubs_and_preserves_unknown_fields() {
        let mut registry = Registry::new();
        let stub_json = CodexProvider
            .provision("sbx-xyz", &sample_chatgpt_real(), &mut registry)
            .expect("provision");

        let parsed: serde_json::Value = serde_json::from_str(&stub_json).unwrap();
        let tokens = parsed.get("tokens").unwrap().as_object().unwrap();
        let access = tokens.get("access_token").and_then(|v| v.as_str()).unwrap();
        let refresh = tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap();

        // Stub format.
        assert!(access.starts_with(STUB_ACCESS_PREFIX), "got {access}");
        assert!(refresh.starts_with(STUB_REFRESH_PREFIX), "got {refresh}");
        let tail_a = access.strip_prefix(STUB_ACCESS_PREFIX).unwrap();
        let tail_r = refresh.strip_prefix(STUB_REFRESH_PREFIX).unwrap();
        assert!(tail_a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(tail_r.chars().all(|c| c.is_ascii_alphanumeric()));
        // Sandbox id encoded (dashes stripped).
        assert!(access.contains("sbxxyz"));

        // id_token + account_id preserved verbatim.
        assert_eq!(
            tokens.get("id_token").and_then(|v| v.as_str()),
            Some(FAKE_JWT)
        );
        assert_eq!(
            tokens.get("account_id").and_then(|v| v.as_str()),
            Some("acct_abc")
        );

        // Real refresh token never appears in stub.
        assert!(!stub_json.contains("rt_opaque_real_xyz"));

        // Outer-level unknown fields preserved.
        assert_eq!(
            parsed.get("future_feature").and_then(|v| v.as_str()),
            Some("preserve_me")
        );
        assert_eq!(
            parsed.get("auth_mode").and_then(|v| v.as_str()),
            Some("ChatGPT")
        );

        // Registry knows about both stubs.
        assert_eq!(registry.sandbox_for_stub(access), Some("sbx-xyz"));
        assert_eq!(registry.sandbox_for_stub(refresh), Some("sbx-xyz"));
    }

    #[test]
    fn provision_apikey_mode_is_rejected_with_clear_message() {
        let mut registry = Registry::new();
        let err = CodexProvider
            .provision("sbx-1", &sample_apikey_real(), &mut registry)
            .unwrap_err();
        assert!(
            err.contains("ApiKey mode") && err.contains("task #26"),
            "expected ApiKey + task #26 hint, got: {err}"
        );
    }

    #[test]
    fn provision_rejects_non_jwt_access_token() {
        let mut registry = Registry::new();
        let mut bad = sample_chatgpt_real();
        bad["tokens"]["access_token"] = serde_json::Value::String("notajwt".into());
        let err = CodexProvider
            .provision("sbx", &bad, &mut registry)
            .unwrap_err();
        assert!(err.contains("JWT"), "got: {err}");
    }

    #[test]
    fn intercept_covers_chatgpt_and_oauth_only() {
        let p = CodexProvider;
        assert!(p.intercept("chatgpt.com"));
        assert!(p.intercept("backend-api.chatgpt.com"));
        assert!(p.intercept("chat.openai.com"));
        assert!(p.intercept("auth.openai.com"));
        // api.openai.com is *not* intercepted in v0.5 — API-key path.
        assert!(!p.intercept("api.openai.com"));
        assert!(!p.intercept("api.anthropic.com"));
        assert!(!p.intercept("openai.com"));
    }

    #[test]
    fn creds_path_is_codex_auth_json() {
        assert_eq!(CodexProvider.creds_path(), Path::new(".codex/auth.json"));
    }

    // ── End-to-end request integration tests ────────────────────────

    use crate::vault::providers::test_support::{
        body_bytes, build_request, cleanup, expect_request, expect_response, fresh_server,
        sample_codex_real,
    };
    use hudsucker::Body;

    fn stubs_for_sandbox(
        server: &crate::vault::server::Server,
        sandbox_id: &str,
    ) -> (String, String) {
        let registry = server.registry_lock_for_test();
        let stubs = registry.stubs_for(sandbox_id).unwrap().to_vec();
        let access = stubs
            .iter()
            .find(|s| s.starts_with(STUB_ACCESS_PREFIX))
            .cloned()
            .expect("access stub present");
        let refresh = stubs
            .iter()
            .find(|s| s.starts_with(STUB_REFRESH_PREFIX))
            .cloned()
            .expect("refresh stub present");
        (access, refresh)
    }

    #[tokio::test]
    async fn bearer_request_swaps_stub_to_real_access_token() {
        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease("codex", "sbx-cx", sample_codex_real())
            .expect("lease");
        let (stub_access, _) = stubs_for_sandbox(&server, "sbx-cx");

        let req = build_request(
            "POST",
            "https://chatgpt.com/backend-api/conversation",
            Body::empty(),
        );
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                "authorization",
                format!("Bearer {stub_access}").parse().unwrap(),
            );
            Request::from_parts(parts, body)
        };

        let out = CodexProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "codex bearer swap");
        let auth = out_req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, format!("Bearer {FAKE_JWT}"));

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn bearer_with_unknown_codex_stub_returns_401() {
        // Stub prefix matches codex's, so it's clearly ours, but it's
        // not in the registry. Provider should reject.
        let (server, dir) = fresh_server().await;
        let req = build_request("POST", "https://chatgpt.com/", Body::empty());
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                "authorization",
                format!("Bearer {STUB_ACCESS_PREFIX}unknownStubSuffix")
                    .parse()
                    .unwrap(),
            );
            Request::from_parts(parts, body)
        };

        let out = CodexProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let res = expect_response(out, "codex unknown stub");
        assert_eq!(res.status(), 401);
        let bytes = body_bytes(res.into_body()).await;
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("unknown codex stub access token"), "body: {s}");

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn bearer_without_codex_prefix_passes_through() {
        // A bearer that doesn't start with `pb-codex-oat-` isn't ours.
        // The codex provider documents that this should pass through —
        // future-proof for overlapping hosts.
        let (server, dir) = fresh_server().await;
        let req = build_request("POST", "https://chatgpt.com/", Body::empty());
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                "authorization",
                "Bearer some-non-codex-token".parse().unwrap(),
            );
            Request::from_parts(parts, body)
        };

        let out = CodexProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "non-codex bearer pass-through");
        assert_eq!(
            out_req
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer some-non-codex-token"
        );

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn bearer_without_auth_header_passes_through() {
        let (server, dir) = fresh_server().await;
        let req = build_request("GET", "https://chatgpt.com/api/health", Body::empty());

        let out = CodexProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "codex no-auth pass-through");
        assert!(out_req.headers().get("authorization").is_none());

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn oauth_refresh_request_is_rejected_without_releasing_token() {
        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease("codex", "sbx-cx-rt", sample_codex_real())
            .expect("lease");
        let (_, stub_refresh) = stubs_for_sandbox(&server, "sbx-cx-rt");
        let req = Request::builder()
            .method("POST")
            .uri("https://auth.openai.com/oauth/token")
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"grant_type":"refresh_token","refresh_token":"{stub_refresh}"}}"#
            )))
            .unwrap();
        let res = expect_response(
            CodexProvider
                .handle_request(req, server.inner_for_test())
                .await,
            "broker-owned codex refresh",
        );
        assert_eq!(res.status(), 403);
        let body = String::from_utf8(body_bytes(res.into_body()).await).unwrap();
        assert!(body.contains("OAuth rotation is broker-owned"));
        assert!(!body.contains(&stub_refresh));
        assert!(!body.contains("rt_codex_real"));

        drop(_lease);
        cleanup(server, dir);
    }
}
