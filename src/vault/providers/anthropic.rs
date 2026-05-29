//! Anthropic provider — Claude Code OAuth.
//!
//! Mirrors the v0.4 single-provider implementation: intercept
//! `api.anthropic.com` (bearer-token swap on every request) and
//! `console.anthropic.com/oauth/token` (refresh-token swap on the way
//! out, real → stub swap + registry rotation on the way back).

use std::path::Path;

use async_trait::async_trait;
use http_body_util::BodyExt;
use hudsucker::{
    hyper::{
        header::{HeaderValue, AUTHORIZATION},
        Request, Response,
    },
    Body, RequestOrResponse,
};
use serde::Deserialize;

use super::{
    host_from_uri, mint_stub, swap_raw_header, unauthorized, ApiKeySwap, ChatInput, PendingFlow,
    Registry, SandboxData, VaultProvider,
};
use crate::vault::server::ServerInner;

// Provider id matches the AgentSpec id (`claude`) so
// `VaultSession::start(agent_id, ...)` can look up the right provider
// directly. The module is named `anthropic` because that's the
// underlying upstream service this provider knows how to swap creds
// for; the *agent* using it happens to be Claude Code.
const PROVIDER_ID: &str = "claude";

const API_HOST: &str = "api.anthropic.com";
const CONSOLE_HOST: &str = "console.anthropic.com";
/// Newer Anthropic OAuth host — claude code refreshes its access token
/// against `platform.claude.com/oauth/token`. Without this in the
/// intercept list the stub refresh token passes through to the real host,
/// gets rejected, and the access-token swap on `api.anthropic.com` then
/// fails with 401 "Invalid authentication credentials" downstream.
const PLATFORM_HOST: &str = "platform.claude.com";
const OAUTH_TOKEN_PATH_SUFFIX: &str = "/oauth/token";
const CREDS_PATH: &str = ".claude/.credentials.json";

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

    fn creds_path(&self) -> &'static Path {
        Path::new(CREDS_PATH)
    }

    fn provision(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
        registry: &mut Registry,
    ) -> Result<String, String> {
        // Validate shape — we want a clear error if the user pointed us at
        // the wrong file rather than a silent passthrough of garbage.
        let oauth = real
            .get("claudeAiOauth")
            .ok_or_else(|| "anthropic creds missing claudeAiOauth field".to_string())?;
        let _block: OauthBlock = serde_json::from_value(oauth.clone())
            .map_err(|error| format!("parse claudeAiOauth: {error}"))?;

        let stub_access = mint_stub(STUB_ACCESS_PREFIX, sandbox_id);
        let stub_refresh = mint_stub(STUB_REFRESH_PREFIX, sandbox_id);

        // Build stub creds JSON by cloning real and overwriting just the
        // token fields. Preserves `expiresAt`, `scopes`, `subscriptionType`,
        // and any future fields so the guest sees a structurally-correct
        // file.
        let mut stub_value = real.clone();
        {
            let oauth = stub_value
                .get_mut("claudeAiOauth")
                .and_then(|v| v.as_object_mut())
                .ok_or_else(|| "claudeAiOauth block missing".to_string())?;
            oauth.insert(
                "accessToken".to_string(),
                serde_json::Value::String(stub_access.clone()),
            );
            oauth.insert(
                "refreshToken".to_string(),
                serde_json::Value::String(stub_refresh.clone()),
            );
        }
        let stub_json = serde_json::to_string_pretty(&stub_value)
            .map_err(|error| format!("serialize stub creds: {error}"))?;

        registry.insert(
            sandbox_id.to_string(),
            SandboxData {
                provider_id: PROVIDER_ID,
                real: real.clone(),
                stubs: vec![stub_access, stub_refresh],
            },
        );

        Ok(stub_json)
    }

    async fn handle_request(
        &self,
        req: Request<Body>,
        server: &ServerInner,
        pending: &mut Option<PendingFlow>,
    ) -> RequestOrResponse {
        let host = host_from_uri(&req).unwrap_or_default();
        // OAuth refresh (`/oauth/token`) needs JSON-body rewriting on
        // both the legacy console host and the newer platform host. Plain
        // bearer-token endpoints (everything else under our intercept
        // set) get the Authorization-header swap.
        let is_oauth_path = req.uri().path().ends_with(OAUTH_TOKEN_PATH_SUFFIX);
        if is_oauth_path && (host == CONSOLE_HOST || host == PLATFORM_HOST) {
            return handle_oauth_request(req, server, pending).await;
        }
        if host == API_HOST || host == PLATFORM_HOST {
            return handle_api_request(req, server).await;
        }
        req.into()
    }

    async fn handle_response(
        &self,
        res: Response<Body>,
        server: &ServerInner,
        pending: &mut Option<PendingFlow>,
    ) -> Response<Body> {
        let Some(flow) = pending.take() else {
            return res;
        };
        if flow.provider_id != PROVIDER_ID {
            // Misrouted; put it back. Shouldn't happen — server.rs only
            // dispatches to the provider that set the flow.
            *pending = Some(flow);
            return res;
        }

        let (parts, body) = res.into_parts();
        let collected = match body.collect().await {
            Ok(c) => c.to_bytes(),
            Err(error) => {
                eprintln!("pillbox: vault: failed to collect oauth response body: {error}");
                return Response::from_parts(parts, Body::empty());
            }
        };

        let mut value: serde_json::Value = match serde_json::from_slice(&collected) {
            Ok(v) => v,
            Err(error) => {
                eprintln!("pillbox: vault: oauth response not JSON; passing through: {error}");
                return Response::from_parts(parts, Body::from(collected));
            }
        };

        // Two passes through the map: first rotate stored real values
        // with whatever the server returned, then read back the stubs to
        // swap into the body.
        let stub_pair = {
            let mut registry = server.registry_lock();
            if let Some(obj) = value.as_object_mut() {
                if let Some(new_access) = obj
                    .get("access_token")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                {
                    registry.rotate_real_field(
                        &flow.sandbox_id,
                        "/claudeAiOauth/accessToken",
                        new_access,
                    );
                }
                if let Some(new_refresh) = obj
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                {
                    registry.rotate_real_field(
                        &flow.sandbox_id,
                        "/claudeAiOauth/refreshToken",
                        new_refresh,
                    );
                }
            }
            stubs_for(&registry, &flow.sandbox_id)
        };

        if let (Some(obj), Some((stub_access, stub_refresh))) =
            (value.as_object_mut(), stub_pair.as_ref())
        {
            if obj.contains_key("access_token") {
                obj.insert(
                    "access_token".to_string(),
                    serde_json::Value::String(stub_access.clone()),
                );
            }
            if obj.contains_key("refresh_token") {
                obj.insert(
                    "refresh_token".to_string(),
                    serde_json::Value::String(stub_refresh.clone()),
                );
            }
        }

        let new_body = serde_json::to_vec(&value).unwrap_or(collected.to_vec());
        let new_len = new_body.len();
        let mut parts = parts;
        parts.headers.remove("content-length");
        parts
            .headers
            .insert("content-length", HeaderValue::from(new_len));
        Response::from_parts(parts, Body::from(new_body))
    }

    /// Anthropic's chat endpoint is `POST /v1/messages` (on both
    /// `api.anthropic.com` and the platform host the handler already
    /// matched). The request body carries the full conversation.
    fn captures_chat_input(&self, method: &str, path: &str) -> bool {
        method == "POST" && path.ends_with("/v1/messages")
    }

    /// Pull `messages` + `system` out of the Anthropic Messages request
    /// body. Both are emitted verbatim (JSON-encoded) as OTel GenAI
    /// conversation attributes — Anthropic's `messages` are already
    /// `[{role, content}]` with content as a string or content-block
    /// array, which the consumer's gen_ai adapter reads directly.
    /// Best-effort: a non-JSON / message-less body yields `None`.
    fn parse_chat_input(&self, body: &[u8]) -> Option<ChatInput> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let messages = value.get("messages").filter(|m| m.is_array())?;
        let input_messages = serde_json::to_string(messages).ok()?;
        let system_instructions = value
            .get("system")
            .map(|s| serde_json::to_string(s).unwrap_or_default());
        Some(ChatInput {
            input_messages,
            system_instructions,
        })
    }
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    _access_token: String,
    #[serde(rename = "refreshToken")]
    _refresh_token: String,
}

async fn handle_oauth_request(
    req: Request<Body>,
    server: &ServerInner,
    pending: &mut Option<PendingFlow>,
) -> RequestOrResponse {
    let (mut parts, body) = req.into_parts();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(error) => {
            eprintln!("pillbox: vault: failed to collect oauth request body: {error}");
            return unauthorized("body read error").into();
        }
    };

    let mut value: serde_json::Value = match serde_json::from_slice(&collected) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON; we can't rewrite. Reject — refusing is safer
            // than leaking the real token by accident.
            return unauthorized("non-json oauth body").into();
        }
    };

    // Two grant types reach /oauth/token:
    //
    // 1. `grant_type=refresh_token` — the agent's normal token refresh.
    //    Body carries a `refresh_token` that's one of our minted stubs;
    //    we look up the sandbox by stub, swap stub → real on the way
    //    out, and the response will return rotated tokens.
    //
    // 2. `grant_type=authorization_code` — Claude Code's `/login`
    //    flow. Body carries a one-time `code` from the user's
    //    browser-side OAuth dance; no stub to look up. We still must
    //    intercept the *response* because that's where the freshly-
    //    minted real tokens come back — without rewriting them to
    //    stubs, the agent stores real bearers and every subsequent
    //    API call 401s when the vault sees an unknown stub.
    //
    // Either way, set `pending` so handle_response runs; the
    // identification path differs.
    let stub_refresh = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let real_refresh = if let Some(stub) = stub_refresh.as_deref() {
        let registry = server.registry_lock();
        let sandbox_id = match registry.sandbox_for_stub(stub) {
            Some(s) => s.to_string(),
            None => return unauthorized("unknown stub refresh token").into(),
        };
        *pending = Some(PendingFlow {
            provider_id: PROVIDER_ID,
            sandbox_id: sandbox_id.clone(),
        });
        registry
            .real(&sandbox_id)
            .and_then(|v| v.pointer("/claudeAiOauth/refreshToken"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    } else {
        // Authorization-code grant: identify the sandbox by the
        // unique anthropic OAuth lease in the registry. Pass the
        // request body through unchanged — the code itself isn't
        // ours to rewrite, and the upstream needs it verbatim to
        // mint the new token pair.
        let sandbox_id = server
            .registry_lock()
            .unique_sandbox_for_provider(PROVIDER_ID);
        if let Some(sandbox_id) = sandbox_id {
            *pending = Some(PendingFlow {
                provider_id: PROVIDER_ID,
                sandbox_id,
            });
        }
        None
    };

    if let (Some(obj), Some(real)) = (value.as_object_mut(), real_refresh) {
        obj.insert("refresh_token".to_string(), serde_json::Value::String(real));
    }

    let new_body = serde_json::to_vec(&value).unwrap_or(collected.to_vec());
    let new_len = new_body.len();
    parts.headers.remove("content-length");
    parts
        .headers
        .insert("content-length", HeaderValue::from(new_len));
    // Force the upstream OAuth response back uncompressed so our
    // response-side `handle_response` can `serde_json::from_slice`
    // it. Anthropic gzips `/oauth/token` responses when the client
    // advertises `Accept-Encoding: gzip` (Claude Code does); the
    // vault used to log "oauth response not JSON; passing through"
    // and the agent ended up with raw tokens that the registry
    // didn't recognize on subsequent API calls → 401.
    parts.headers.remove("accept-encoding");
    parts
        .headers
        .insert("accept-encoding", HeaderValue::from_static("identity"));
    Request::from_parts(parts, Body::from(new_body)).into()
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
    // Force identity encoding so the gen_ai response tap reads plaintext
    // SSE. Anthropic gzips `/v1/messages` responses when the client sends
    // `accept-encoding: gzip` (Claude Code does), which leaves the tap
    // parsing compressed bytes — no usage, no output messages. Requesting
    // identity is invisible to the agent (any client accepts uncompressed)
    // and is the same trick the OAuth path already uses.
    req.headers_mut().remove("accept-encoding");
    req.headers_mut()
        .insert("accept-encoding", HeaderValue::from_static("identity"));

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
    let (mut parts, body) = req.into_parts();
    let Some(header_value) = parts.headers.get(X_API_KEY_HEADER).cloned() else {
        return Request::from_parts(parts, body).into();
    };
    match swap_raw_header(&header_value, server) {
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

fn stubs_for(registry: &Registry, sandbox_id: &str) -> Option<(String, String)> {
    let stubs = registry.stubs_for(sandbox_id)?;
    let access = stubs
        .iter()
        .find(|s| s.starts_with(STUB_ACCESS_PREFIX))?
        .clone();
    let refresh = stubs
        .iter()
        .find(|s| s.starts_with(STUB_REFRESH_PREFIX))?
        .clone();
    Some((access, refresh))
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
        assert_eq!(
            oauth.get("expiresAt").and_then(|v| v.as_u64()),
            Some(1700000000)
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
                    "value": "sk-ant-api03-REAL-secret"
                }),
                stubs: vec![stub.into()],
            },
        );
        // The Anthropic provider should look this up via
        // `api_key_real_for_stub` even though the entry was minted
        // outside the OAuth `provision` path.
        assert_eq!(
            r.api_key_real_for_stub(stub),
            Some("sk-ant-api03-REAL-secret"),
        );
        // OAuth-style real lookup should NOT pick it up (different shape).
        assert!(r
            .real("sbx-apikey")
            .and_then(|v| v.pointer("/claudeAiOauth/accessToken"))
            .is_none());
    }

    // ── End-to-end Request/Response integration tests ────────────────
    //
    // These tests construct hyper `Request<Body>` / `Response<Body>`
    // objects, hand them to the provider's handlers, and assert on the
    // returned objects. They live in the crate test module (rather than
    // a separate `tests/` integration crate) because the trait methods
    // and supporting types are `pub(crate)`.

    use crate::vault::known_secrets::HeaderScheme;
    use crate::vault::providers::test_support::{
        body_bytes, body_json, build_json_response, build_request, cleanup, expect_request,
        expect_response, fresh_server, sample_anthropic_real,
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

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "bearer swap");

        let auth = out_req
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer REAL_ACCESS");
        // The bearer flow doesn't need a response-side swap.
        assert!(pending.is_none());

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

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let res = expect_response(out, "unknown bearer");
        assert_eq!(res.status(), 401);
        let body = body_bytes(res.into_body()).await;
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("\"vault\":\"unauthorized\""), "body: {s}");
        assert!(s.contains("unknown stub access token"), "body: {s}");
        assert!(pending.is_none());

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn bearer_request_without_auth_header_passes_through() {
        let (server, dir) = fresh_server().await;
        let req = build_request("GET", "https://api.anthropic.com/v1/models", Body::empty());

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "no-auth pass-through");
        assert!(out_req.headers().get("authorization").is_none());
        assert!(pending.is_none());

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

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
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

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
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
    async fn oauth_refresh_request_swaps_body_and_sets_pending() {
        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease("claude", "sbx-oauth", sample_anthropic_real())
            .expect("lease");
        let (_, stub_refresh) = stubs_for_sandbox(&server, "sbx-oauth");

        let body_json_in = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": stub_refresh,
        });
        let body_bytes_in = serde_json::to_vec(&body_json_in).unwrap();
        let len = body_bytes_in.len();
        let req = Request::builder()
            .method("POST")
            .uri("https://console.anthropic.com/oauth/token")
            .header("content-type", "application/json")
            .header("content-length", len)
            .body(Body::from(body_bytes_in))
            .unwrap();

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "oauth refresh request");

        let body = body_json(out_req.into_body()).await;
        assert_eq!(
            body.get("refresh_token").and_then(|v| v.as_str()),
            Some("REAL_REFRESH")
        );
        let flow = pending.expect("pending should be set for oauth refresh");
        assert_eq!(flow.provider_id, "claude");
        assert_eq!(flow.sandbox_id, "sbx-oauth");

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn oauth_authorization_code_grant_sets_pending_for_unique_sandbox() {
        // Claude Code's `/login` flow does grant_type=authorization_code
        // with a `code` instead of a refresh_token. The vault still needs
        // to intercept the response (which carries the freshly-minted
        // real tokens) so it can rotate vault state + swap real → stub
        // in the body — otherwise the agent stores real bearers and
        // every subsequent API call 401s when the vault sees an unknown
        // stub. Regression guard for that path.
        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease("claude", "sbx-fresh-login", sample_anthropic_real())
            .expect("lease");

        let body_json_in = serde_json::json!({
            "grant_type": "authorization_code",
            "code": "oauth_code_from_browser",
            "client_id": "claude_code",
            "redirect_uri": "http://localhost:54321/callback",
        });
        let body_bytes_in = serde_json::to_vec(&body_json_in).unwrap();
        let len = body_bytes_in.len();
        let req = Request::builder()
            .method("POST")
            .uri("https://console.anthropic.com/oauth/token")
            .header("content-type", "application/json")
            .header("content-length", len)
            .body(Body::from(body_bytes_in.clone()))
            .unwrap();

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let out_req = expect_request(out, "oauth authorization-code request");

        // The request body passes through unchanged — we don't own
        // the authorization code and the upstream needs it verbatim.
        let bytes = body_bytes(out_req.into_body()).await;
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body.get("grant_type").and_then(|v| v.as_str()),
            Some("authorization_code")
        );
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("oauth_code_from_browser")
        );

        // But pending MUST be set so handle_response runs and swaps the
        // real tokens to stubs on the way back to the agent.
        let flow = pending.expect("pending should be set for authorization_code grant");
        assert_eq!(flow.provider_id, "claude");
        assert_eq!(flow.sandbox_id, "sbx-fresh-login");

        drop(_lease);
        cleanup(server, dir);
    }

    #[tokio::test]
    async fn oauth_refresh_request_unknown_stub_returns_401_and_no_pending() {
        let (server, dir) = fresh_server().await;
        let body_json_in = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": "sk-ant-ort01-unknownStubRefresh",
        });
        let body_bytes_in = serde_json::to_vec(&body_json_in).unwrap();
        let len = body_bytes_in.len();
        let req = Request::builder()
            .method("POST")
            .uri("https://console.anthropic.com/oauth/token")
            .header("content-type", "application/json")
            .header("content-length", len)
            .body(Body::from(body_bytes_in))
            .unwrap();

        let mut pending: Option<PendingFlow> = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let res = expect_response(out, "oauth unknown stub");
        assert_eq!(res.status(), 401);
        assert!(pending.is_none());

        cleanup(server, dir);
    }

    #[tokio::test]
    async fn oauth_refresh_response_swaps_real_to_new_stubs_and_rotates_registry() {
        let (server, dir) = fresh_server().await;
        let _lease = server
            .lease("claude", "sbx-rot", sample_anthropic_real())
            .expect("lease");
        let (stub_access_before, stub_refresh_before) = stubs_for_sandbox(&server, "sbx-rot");

        // The response handler only fires when pending was set by a
        // prior request-side swap. Mirror that here.
        let mut pending: Option<PendingFlow> = Some(PendingFlow {
            provider_id: "claude",
            sandbox_id: "sbx-rot".into(),
        });

        let res = build_json_response(serde_json::json!({
            "access_token": "NEW_REAL_ACCESS",
            "refresh_token": "NEW_REAL_REFRESH",
            "expires_in": 3600,
        }));

        let out = AnthropicProvider
            .handle_response(res, server.inner_for_test(), &mut pending)
            .await;

        // Pending must be cleared so the next request/response pair
        // starts fresh.
        assert!(pending.is_none());

        let (parts, body) = out.into_parts();
        // content-length must reflect the rewritten body, not the old one.
        let cl: usize = parts
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .expect("content-length present and numeric");
        let raw_body = body_bytes(body).await;
        assert_eq!(cl, raw_body.len(), "content-length matches body length");

        let body: serde_json::Value = serde_json::from_slice(&raw_body).unwrap();
        let access = body.get("access_token").and_then(|v| v.as_str()).unwrap();
        let refresh = body.get("refresh_token").and_then(|v| v.as_str()).unwrap();
        // Body must carry stubs back to the guest, not the new real tokens.
        assert!(access.starts_with(STUB_ACCESS_PREFIX), "got {access}");
        assert!(refresh.starts_with(STUB_REFRESH_PREFIX), "got {refresh}");
        assert!(!raw_body.windows(15).any(|w| w == b"NEW_REAL_ACCESS"));
        assert!(!raw_body.windows(16).any(|w| w == b"NEW_REAL_REFRESH"));

        // Stubs minted at provision time stay stable across rotation —
        // pillbox rotates only the *real* side, not the stub the guest sees.
        let (stub_access_after, stub_refresh_after) = stubs_for_sandbox(&server, "sbx-rot");
        assert_eq!(stub_access_before, stub_access_after);
        assert_eq!(stub_refresh_before, stub_refresh_after);

        // Registry's stored real values must have rotated.
        {
            let registry = server.registry_lock_for_test();
            let real = registry.real("sbx-rot").unwrap();
            assert_eq!(
                real.pointer("/claudeAiOauth/accessToken")
                    .and_then(|v| v.as_str()),
                Some("NEW_REAL_ACCESS")
            );
            assert_eq!(
                real.pointer("/claudeAiOauth/refreshToken")
                    .and_then(|v| v.as_str()),
                Some("NEW_REAL_REFRESH")
            );
        }

        drop(_lease);
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
            let mut pending = None;
            let out = AnthropicProvider
                .handle_request(req, server.inner_for_test(), &mut pending)
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
        let mut pending = None;
        let out = AnthropicProvider
            .handle_request(req, server.inner_for_test(), &mut pending)
            .await;
        let res = expect_response(out, "post-drop 401");
        assert_eq!(res.status(), 401);

        cleanup(server, dir);
    }
}
