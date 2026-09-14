//! Anthropic provider — Claude Code OAuth.
//!
//! Intercepts Anthropic API traffic for bearer-token substitution. OAuth token
//! endpoints are intercepted only to reject guest-owned rotation locally; the
//! host broker is the sole rotation authority.

use std::path::Path;

use async_trait::async_trait;
use hudsucker::{
    hyper::{
        header::{HeaderValue, AUTHORIZATION},
        Request,
    },
    Body, RequestOrResponse,
};
use serde::Deserialize;

use super::{
    host_from_uri, mint_stub, oauth_rotation_forbidden, swap_raw_header, unauthorized, ApiKeySwap,
    HostProxyOAuthStub, OAuthCodec, OAuthRefreshRequest, Registry, SandboxData, VaultProvider,
};
#[cfg(feature = "libkrun")]
use super::{LibkrunOAuthStub, OAuthRelease};
#[cfg(not(feature = "libkrun"))]
use crate::vault::refresh::STUB_FAR_FUTURE_EXPIRES_AT_MS as STUB_EXPIRES_AT_MS;
use crate::vault::server::ServerInner;
use crate::vault::token_store::RefreshDecider;
#[cfg(feature = "libkrun")]
use crate::vault::STUB_FAR_FUTURE_EXPIRES_AT_MS as STUB_EXPIRES_AT_MS;

// Provider id matches the AgentSpec id (`claude`) so
// `VaultSession::start(agent_id, ...)` can look up the right provider
// directly. The module is named `anthropic` because that's the
// underlying upstream service this provider knows how to swap creds
// for; the *agent* using it happens to be Claude Code.
const PROVIDER_ID: &str = "claude";

const API_HOST: &str = "api.anthropic.com";
const CONSOLE_HOST: &str = "console.anthropic.com";
/// Newer Anthropic OAuth host. It remains intercepted so the guest token
/// endpoint can be rejected locally instead of reaching the provider.
const PLATFORM_HOST: &str = "platform.claude.com";
const OAUTH_TOKEN_PATH_SUFFIX: &str = "/oauth/token";
const CREDS_PATH: &str = ".claude/.credentials.json";
const OAUTH_CLIENT_ID: &str = "claude_code";
const OAUTH_ENDPOINT: &str = "https://platform.claude.com/oauth/token";
const FALLBACK_EXPIRES_IN_SECS: u64 = 3600;

// Stub tokens mimic Anthropic's `sk-ant-oat01-` / `sk-ant-ort01-`
// prefixes so Claude Code's local format validation accepts them. The
// suffix is pure alphanumeric (no dashes/underscores) for the same
// reason. Anthropic doesn't see these — by the time a request hits the
// wire the proxy has swapped them for the real values.
pub(crate) const STUB_ACCESS_PREFIX: &str = "sk-ant-oat01-";
pub(crate) const STUB_REFRESH_PREFIX: &str = "sk-ant-ort01-";

pub(crate) struct AnthropicProvider;

#[async_trait]
impl VaultProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn intercept(&self, host: &str) -> bool {
        host == API_HOST || host == CONSOLE_HOST || host == PLATFORM_HOST
    }

    fn hosts(&self) -> &'static [&'static str] {
        &[API_HOST, CONSOLE_HOST, PLATFORM_HOST]
    }

    fn creds_path(&self) -> &'static Path {
        Path::new(CREDS_PATH)
    }

    fn oauth_codec(&self) -> Option<&dyn OAuthCodec> {
        Some(self)
    }

    fn is_oauth_token_endpoint(&self, host: &str, path: &str) -> bool {
        (host == CONSOLE_HOST || host == PLATFORM_HOST) && path.ends_with(OAUTH_TOKEN_PATH_SUFFIX)
    }

    fn provision(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
        registry: &mut Registry,
    ) -> Result<String, String> {
        let stub = self.host_proxy_stub(sandbox_id, real)?;
        let stub_json = serde_json::to_string_pretty(&stub.credentials)
            .map_err(|error| format!("serialize stub creds: {error}"))?;

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
        // Both legacy and current token endpoints are local deny points. Plain
        // bearer-token endpoints get the Authorization-header swap.
        if self.is_oauth_token_endpoint(&host, req.uri().path()) {
            return oauth_rotation_forbidden().into();
        }
        if host == API_HOST || host == PLATFORM_HOST {
            return handle_api_request(req, server).await;
        }
        req.into()
    }

    /// Anthropic's generation endpoint is `POST /v1/messages` (on both
    /// `api.anthropic.com` and the platform host the handler already
    /// matched). `ends_with` matches the streaming + non-streaming
    /// generation calls and excludes `…/v1/messages/count_tokens` and
    /// `…/batches`, which aren't generations. Gates the gen_ai *usage*
    /// span to real generation calls. (Conversation content comes from
    /// the transcript synthesizer, not this provider.)
    fn is_chat_request(&self, method: &str, path: &str) -> bool {
        method == "POST" && path.ends_with("/v1/messages")
    }
}

impl RefreshDecider for AnthropicProvider {
    fn needs_refresh(&self, creds: &serde_json::Value) -> bool {
        self.expiry_ms(creds)
            .map(crate::vault::refresh::is_expired)
            .unwrap_or(false)
    }

    fn refresh_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/claudeAiOauth/refreshToken")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn access_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/claudeAiOauth/accessToken")
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

impl OAuthCodec for AnthropicProvider {
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
        let expires_in = response
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(FALLBACK_EXPIRES_IN_SECS);
        let oauth = real
            .get_mut("claudeAiOauth")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| {
                crate::errors::PillboxError::runtime("vault", "claudeAiOauth block disappeared")
            })?;
        oauth.insert("accessToken".into(), access.into());
        oauth.insert("refreshToken".into(), refresh.into());
        oauth.insert(
            "expiresAt".into(),
            serde_json::Value::Number(serde_json::Number::from(
                now_ms.saturating_add(expires_in.saturating_mul(1000)),
            )),
        );
        Ok(())
    }

    fn expiry_ms(&self, real: &serde_json::Value) -> Option<u64> {
        real.pointer("/claudeAiOauth/expiresAt")
            .and_then(|v| v.as_u64())
            .map(crate::vault::refresh::normalize_expiry_ms)
    }

    fn host_proxy_stub(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
    ) -> Result<HostProxyOAuthStub, String> {
        let oauth = real
            .get("claudeAiOauth")
            .ok_or_else(|| "anthropic creds missing claudeAiOauth field".to_string())?;
        let _block: OauthBlock = serde_json::from_value(oauth.clone())
            .map_err(|error| format!("parse claudeAiOauth: {error}"))?;
        let access = mint_stub(STUB_ACCESS_PREFIX, sandbox_id);
        let refresh = mint_stub(STUB_REFRESH_PREFIX, sandbox_id);
        let mut credentials = real.clone();
        let oauth = credentials
            .get_mut("claudeAiOauth")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| "claudeAiOauth block missing".to_string())?;
        oauth.insert("accessToken".into(), access.clone().into());
        oauth.insert("refreshToken".into(), refresh.clone().into());
        oauth.insert(
            "expiresAt".into(),
            serde_json::Value::Number(serde_json::Number::from(STUB_EXPIRES_AT_MS)),
        );
        Ok(HostProxyOAuthStub {
            credentials,
            stubs: vec![access, refresh],
        })
    }

    #[cfg(feature = "libkrun")]
    fn libkrun_stub(&self, real: &mut serde_json::Value) -> LibkrunOAuthStub {
        let Some(oauth) = real
            .get_mut("claudeAiOauth")
            .and_then(|v| v.as_object_mut())
        else {
            return LibkrunOAuthStub {
                access_stub: None,
                releases: Vec::new(),
            };
        };
        let mut releases = Vec::new();
        let mut access_stub = None;
        for field in ["accessToken", "refreshToken"] {
            let Some(token) = oauth
                .get(field)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            let stub = mint_libkrun_stub(&token);
            oauth.insert(field.into(), stub.clone().into());
            if field == "accessToken" {
                access_stub = Some(stub.clone());
            }
            // The guest needs a shape-valid refresh stub in its file, but only the
            // access stub is releasable on egress. Refresh rotation belongs to the
            // broker and token-endpoint requests are locally rejected.
            if field == "accessToken" {
                releases.push(OAuthRelease { stub, real: token });
            }
        }
        if !releases.is_empty() {
            oauth.insert(
                "expiresAt".into(),
                serde_json::Value::Number(serde_json::Number::from(STUB_EXPIRES_AT_MS)),
            );
        }
        LibkrunOAuthStub {
            access_stub,
            releases,
        }
    }
}

#[cfg(feature = "libkrun")]
fn mint_libkrun_stub(real: &str) -> String {
    let prefix = if real.split('-').count() >= 4 {
        real.splitn(4, '-').take(3).collect::<Vec<_>>().join("-")
    } else {
        "pllbx".to_string()
    };
    format!("{prefix}-pllbxstub{}", uuid::Uuid::now_v7().simple())
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    _access_token: String,
    #[serde(rename = "refreshToken")]
    _refresh_token: String,
}

/// Two flows can target `api.anthropic.com`:
///  - Claude Code's OAuth path: `Authorization: Bearer <stub>` (the
///    accessToken family minted by `provision`).
///  - A `--with ANTHROPIC_API_KEY --vault`'d request: `x-api-key: <stub>`
///    minted by `Server::lease_api_key`.
///
/// We dispatch by which header is present. If neither carries one of
/// ours, we pass through (lets unvaulted `--with` requests keep working).
async fn handle_api_request(mut req: Request<Body>, server: &ServerInner) -> RequestOrResponse {
    // Force identity encoding ONLY on the generation endpoint, so the
    // gen_ai response tap reads plaintext SSE. Anthropic gzips
    // `/v1/messages` responses when the client sends `accept-encoding:
    // gzip` (Claude Code does), which leaves the tap parsing compressed
    // bytes — no usage, no output messages. Requesting identity is
    // invisible to the agent (any client accepts uncompressed) and is the
    // same trick the OAuth path uses. Gated to the chat endpoint so
    // unrelated traffic (count_tokens, models, plain `--with` API-key
    // calls the tap never inspects) keeps whatever compression it
    // negotiated.
    if req.method().as_str() == "POST" && req.uri().path().ends_with("/v1/messages") {
        req.headers_mut().remove("accept-encoding");
        req.headers_mut()
            .insert("accept-encoding", HeaderValue::from_static("identity"));
    }

    let has_x_api_key = req.headers().get(X_API_KEY_HEADER).is_some();
    if has_x_api_key {
        return handle_api_request_x_api_key(req, server).await;
    }
    handle_api_request_bearer(req, server).await
}

const X_API_KEY_HEADER: &str = "x-api-key";

async fn handle_api_request_bearer(req: Request<Body>, server: &ServerInner) -> RequestOrResponse {
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

    let real_access = {
        let registry = server.registry_lock();
        let sandbox_id = match registry.sandbox_for_stub(stub) {
            Some(s) => s.to_string(),
            None => return unauthorized("unknown stub access token").into(),
        };
        registry
            .real(&sandbox_id)
            .and_then(|v| v.pointer("/claudeAiOauth/accessToken"))
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
                eprintln!("pillbox: vault: invalid real access token header: {error}");
                return unauthorized("invalid real token").into();
            }
        }
    }

    Request::from_parts(parts, body).into()
}

async fn handle_api_request_x_api_key(
    req: Request<Body>,
    server: &ServerInner,
) -> RequestOrResponse {
    let host = host_from_uri(&req).unwrap_or_default();
    let (mut parts, body) = req.into_parts();
    let Some(header_value) = parts.headers.get(X_API_KEY_HEADER).cloned() else {
        return Request::from_parts(parts, body).into();
    };
    match swap_raw_header(&header_value, server, &host) {
        ApiKeySwap::Swapped(hv) => {
            parts.headers.insert(X_API_KEY_HEADER, hv);
            Request::from_parts(parts, body).into()
        }
        // Pass through preserves the unvaulted `--with ANTHROPIC_API_KEY`
        // path (real key already in place, nothing to swap).
        ApiKeySwap::PassThrough => Request::from_parts(parts, body).into(),
        ApiKeySwap::Unauthorized(detail) => unauthorized(detail).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_real() -> serde_json::Value {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "REAL_ACCESS",
                "refreshToken": "REAL_REFRESH",
                "expiresAt": 1700000000_u64,
                "subscriptionType": "pro"
            }
        })
    }

    #[test]
    fn provision_registers_stubs_and_swaps_tokens_in_returned_json() {
        let mut registry = Registry::new();
        let stub_json = AnthropicProvider
            .provision("sbx-abc", &sample_real(), &mut registry)
            .expect("provision");

        let parsed: serde_json::Value = serde_json::from_str(&stub_json).unwrap();
        let oauth = parsed.get("claudeAiOauth").unwrap();
        let access = oauth.get("accessToken").and_then(|v| v.as_str()).unwrap();
        let refresh = oauth.get("refreshToken").and_then(|v| v.as_str()).unwrap();

        // Stub format.
        assert!(access.starts_with(STUB_ACCESS_PREFIX));
        assert!(refresh.starts_with(STUB_REFRESH_PREFIX));
        let tail_a = access.strip_prefix(STUB_ACCESS_PREFIX).unwrap();
        let tail_r = refresh.strip_prefix(STUB_REFRESH_PREFIX).unwrap();
        assert!(tail_a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(tail_r.chars().all(|c| c.is_ascii_alphanumeric()));
        // Sandbox id encoded in the tail (dashes stripped).
        assert!(access.contains("sbxabc"));
        assert!(refresh.contains("sbxabc"));

        // Real tokens never appear in the stub.
        assert!(!stub_json.contains("REAL_ACCESS"));
        assert!(!stub_json.contains("REAL_REFRESH"));

        // Unknown fields preserved.
        assert_eq!(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            Some("pro")
        );
        // Broker move: the stub's expiry is post-dated to the far-future sentinel
        // (NOT the real `1700000000`), so the guest never refreshes itself.
        assert_eq!(
            oauth.get("expiresAt").and_then(|v| v.as_u64()),
            Some(STUB_EXPIRES_AT_MS)
        );

        // Registry knows about both stubs.
        assert_eq!(registry.sandbox_for_stub(access), Some("sbx-abc"));
        assert_eq!(registry.sandbox_for_stub(refresh), Some("sbx-abc"));
    }

    #[test]
    fn provision_rejects_missing_oauth_block() {
        let mut registry = Registry::new();
        let bad = serde_json::json!({"apiKey": "sk-ant-real"});
        let err = AnthropicProvider
            .provision("sbx-1", &bad, &mut registry)
            .unwrap_err();
        assert!(err.contains("claudeAiOauth"), "got: {err}");
    }

    #[test]
    fn intercept_matches_anthropic_hosts_only() {
        let p = AnthropicProvider;
        assert!(p.intercept("api.anthropic.com"));
        assert!(p.intercept("console.anthropic.com"));
        assert!(p.intercept("platform.claude.com"));
        assert!(!p.intercept("anthropic.com"));
        assert!(!p.intercept("claude.com"));
        assert!(!p.intercept("chatgpt.com"));
    }

    #[test]
    fn creds_path_is_claude_credentials() {
        assert_eq!(
            AnthropicProvider.creds_path(),
            Path::new(".claude/.credentials.json")
        );
    }

    #[test]
    fn api_key_branch_resolves_via_registry() {
        use crate::vault::providers::{SandboxData, API_KEY_PROVIDER_ID};

        let mut r = Registry::new();
        let stub = "sk-ant-api03-stubvalue";
        r.insert(
            "sbx-apikey".into(),
            SandboxData {
                provider_id: API_KEY_PROVIDER_ID,
                real: serde_json::json!({
                    "name": "ANTHROPIC_API_KEY",
                    "value": "sk-ant-api03-REAL-secret",
                    "host": "api.anthropic.com"
                }),
                stubs: vec![stub.into()],
            },
        );
        // The Anthropic provider should look this up via
        // `api_key_real_for_stub` even though the entry was minted
        // outside the OAuth `provision` path.
        assert_eq!(
            r.api_key_real_for_stub(stub, "api.anthropic.com"),
            Some("sk-ant-api03-REAL-secret"),
        );
        // OAuth-style real lookup should NOT pick it up (different shape).
        assert!(r
            .real("sbx-apikey")
            .and_then(|v| v.pointer("/claudeAiOauth/accessToken"))
            .is_none());
    }

    // ── End-to-end request integration tests ────────────────────────
    //
    // These tests construct hyper `Request<Body>` objects, hand them to the
    // provider handler, and assert on the returned objects. They live in the crate test module (rather than
    // a separate `tests/` integration crate) because the trait methods
    // and supporting types are `pub(crate)`.

    use crate::vault::known_secrets::HeaderScheme;
    use crate::vault::providers::test_support::{
        body_bytes, build_request, cleanup, expect_request, expect_response, fresh_server,
        sample_anthropic_real,
    };
    use hudsucker::Body;

    /// Pull the access + refresh stubs out of the registry for a sandbox
    /// id. Tests use this right after `Server::lease("claude", ..)` to
    /// learn the stubs the provider minted.
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
            .lease("claude", "sbx-int", sample_anthropic_real())
            .expect("lease");
        let (stub_access, _stub_refresh) = stubs_for_sandbox(&server, "sbx-int");

        let req = build_request(
            "POST",
            "https://api.anthropic.com/v1/messages",
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

        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "bearer swap");

        let auth = out_req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer REAL_ACCESS");

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn bearer_request_with_unknown_stub_returns_401() {
        let (server, dir) = fresh_server().await;
        let req = build_request(
            "POST",
            "https://api.anthropic.com/v1/messages",
            Body::empty(),
        );
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert(
                "authorization",
                "Bearer sk-ant-oat01-unknownStubValue".parse().unwrap(),
            );
            Request::from_parts(parts, body)
        };

        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let res = expect_response(out, "unknown bearer");
        assert_eq!(res.status(), 401);
        let body = body_bytes(res.into_body()).await;
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("\"vault\":\"unauthorized\""), "body: {s}");
        assert!(s.contains("unknown stub access token"), "body: {s}");

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn bearer_request_without_auth_header_passes_through() {
        let (server, dir) = fresh_server().await;
        let req = build_request("GET", "https://api.anthropic.com/v1/models", Body::empty());

        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "no-auth pass-through");
        assert!(out_req.headers().get("authorization").is_none());

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn x_api_key_request_swaps_stub_to_real_value() {
        let (server, dir) = fresh_server().await;
        let (_api_lease, stub) = server
            .lease_api_key_for_test(
                "ANTHROPIC_API_KEY",
                "sk-ant-api03-REAL-secret",
                "api.anthropic.com",
                HeaderScheme::XApiKey,
                "sk-ant-api03-",
            )
            .expect("lease api key");

        let req = build_request(
            "POST",
            "https://api.anthropic.com/v1/messages",
            Body::empty(),
        );
        let req = {
            let (mut parts, body) = req.into_parts();
            parts.headers.insert("x-api-key", stub.parse().unwrap());
            Request::from_parts(parts, body)
        };

        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "x-api-key swap");
        let key = out_req
            .headers()
            .get("x-api-key")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(key, "sk-ant-api03-REAL-secret");

        drop(_api_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn x_api_key_unknown_stub_passes_through() {
        // For x-api-key, an unknown value is treated as a `--with`'d
        // real key (no vault meta) — the provider should pass it
        // through rather than 401. This is documented on `ApiKeySwap::PassThrough`.
        let (server, dir) = fresh_server().await;
        let req = build_request(
            "POST",
            "https://api.anthropic.com/v1/messages",
            Body::empty(),
        );
        let req = {
            let (mut parts, body) = req.into_parts();
            parts
                .headers
                .insert("x-api-key", "sk-ant-api03-not-a-stub".parse().unwrap());
            Request::from_parts(parts, body)
        };

        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let out_req = expect_request(out, "x-api-key pass-through");
        assert_eq!(
            out_req
                .headers()
                .get("x-api-key")
                .unwrap()
                .to_str()
                .unwrap(),
            "sk-ant-api03-not-a-stub"
        );

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn oauth_refresh_request_is_rejected_without_inspecting_or_releasing_token() {
        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease("claude", "sbx-oauth", sample_anthropic_real())
            .expect("lease");
        let (_, stub_refresh) = stubs_for_sandbox(&server, "sbx-oauth");
        let req = Request::builder()
            .method("POST")
            .uri("https://console.anthropic.com/oauth/token")
            .header("content-type", "application/json")
            .body(Body::from(format!(
                r#"{{"grant_type":"refresh_token","refresh_token":"{stub_refresh}"}}"#
            )))
            .unwrap();
        let res = expect_response(
            AnthropicProvider
                .handle_request(req, server.inner_for_test())
                .await,
            "broker-owned oauth refresh",
        );
        assert_eq!(res.status(), 403);
        let body = String::from_utf8(body_bytes(res.into_body()).await).unwrap();
        assert!(body.contains("OAuth rotation is broker-owned"));
        assert!(!body.contains(&stub_refresh));
        assert!(!body.contains("REAL_REFRESH"));

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn oauth_authorization_code_grant_is_also_rejected() {
        let (server, dir) = fresh_server().await;
        let req = Request::builder()
            .method("POST")
            .uri("https://platform.claude.com/v1/oauth/token")
            .body(Body::from("not even json"))
            .unwrap();
        let res = expect_response(
            AnthropicProvider
                .handle_request(req, server.inner_for_test())
                .await,
            "guest authorization-code grant",
        );
        assert_eq!(res.status(), 403);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn lease_drop_makes_stub_stop_resolving() {
        let (server, dir) = fresh_server().await;
        let lease = server
            .lease("claude", "sbx-drop", sample_anthropic_real())
            .expect("lease");
        let (stub_access, _stub_refresh) = stubs_for_sandbox(&server, "sbx-drop");

        // Sanity: stub resolves before drop.
        {
            let req = build_request(
                "POST",
                "https://api.anthropic.com/v1/messages",
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
            let out = AnthropicProvider
                .handle_request(req, server.inner_for_test())
                .await;
            let _ = expect_request(out, "pre-drop swap");
        }

        // Drop the lease — the registry mapping is removed.
        drop(lease);

        // Same stub now 401s.
        let req = build_request(
            "POST",
            "https://api.anthropic.com/v1/messages",
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
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test())
            .await;
        let res = expect_response(out, "post-drop 401");
        assert_eq!(res.status(), 401);

        cleanup(server, dir);
    }
}
