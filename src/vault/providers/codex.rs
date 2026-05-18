//! Codex provider — OpenAI ChatGPT-mode OAuth.
//!
//! v0.5 scope: ChatGPT mode only. The codex `auth.json` schema is:
//!
//! ```jsonc
//! {
//!   "auth_mode": "ChatGPT",
//!   "tokens": {
//!     "id_token": "<JWT>",        // identity claims; copied verbatim into the stub
//!     "access_token": "<JWT>",    // swapped
//!     "refresh_token": "<opaque>",// swapped
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
//!  - `auth.openai.com` — `/oauth/token` is the refresh endpoint
//!    (verified against `codex-rs/login/src/auth/manager.rs`:
//!    `const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token"`).
//!
//! `api.openai.com` is **not** intercepted in v0.5 — that's the API-key
//! path, deferred until the ApiKey vault track lands.

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

use super::{
    host_from_uri, mint_stub, unauthorized, PendingFlow, Registry, SandboxData, VaultProvider,
};
use crate::vault::server::ServerInner;

const PROVIDER_ID: &str = "codex";

const CHATGPT_HOST: &str = "chatgpt.com";
const CHATGPT_HOST_DOT: &str = ".chatgpt.com";
const CHAT_OPENAI_HOST: &str = "chat.openai.com";
const AUTH_OPENAI_HOST: &str = "auth.openai.com";
const OAUTH_TOKEN_PATH_SUFFIX: &str = "/oauth/token";
const CREDS_PATH: &str = ".codex/auth.json";

// Codex doesn't ship a public stub prefix convention (tokens are opaque
// JWTs / random strings). We invent a `pb-codex-` family so:
//  - the proxy can recognise stubs without parsing them, and
//  - if a stub ever leaks into a log somewhere it's obviously a pillbox
//    artifact, not a real OpenAI token.
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

    fn creds_path(&self) -> &'static Path {
        Path::new(CREDS_PATH)
    }

    fn provision(
        &self,
        sandbox_id: &str,
        real: &serde_json::Value,
        registry: &mut Registry,
    ) -> Result<String, String> {
        let obj = real
            .as_object()
            .ok_or_else(|| "codex auth.json must be a JSON object".to_string())?;

        // Reject ApiKey mode — the swap pipeline needs a refresh token to
        // intercept rotation, which API-key auth doesn't have.
        let tokens = match obj.get("tokens") {
            Some(serde_json::Value::Object(map)) => map,
            Some(serde_json::Value::Null) | None => {
                return Err(
                    "codex auth.json has no `tokens` block (ApiKey mode). \
                     v0.5 vault supports ChatGPT mode only. \
                     The API-key path lands with the API-key vault track \
                     (pillbox task #26)."
                        .into(),
                );
            }
            Some(_) => return Err("codex auth.json `tokens` is not an object".into()),
        };

        let real_access = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "codex auth.json: tokens.access_token missing".to_string())?;
        let _real_refresh = tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "codex auth.json: tokens.refresh_token missing".to_string())?;

        // Sanity check: access_token should look like a JWT (three
        // dot-separated parts). If not, codex's own validator will choke
        // anyway, but bail early with a clearer message.
        if real_access.split('.').count() != 3 {
            return Err(
                "codex auth.json: tokens.access_token is not a JWT (expected 3 dot-separated parts)"
                    .into(),
            );
        }

        let stub_access = mint_stub(STUB_ACCESS_PREFIX, sandbox_id);
        let stub_refresh = mint_stub(STUB_REFRESH_PREFIX, sandbox_id);

        // Build the stub auth.json by cloning and swapping just the two
        // token fields. id_token is left verbatim — it's a self-contained
        // JWT used for identity claims (account_id, plan_type, email),
        // not a bearer credential, so preserving it keeps the guest's
        // account identity intact without exposing anything codex
        // wouldn't already accept.
        let mut stub_value = real.clone();
        {
            let tokens = stub_value
                .get_mut("tokens")
                .and_then(|v| v.as_object_mut())
                .ok_or_else(|| "tokens block missing during stub build".to_string())?;
            tokens.insert(
                "access_token".to_string(),
                serde_json::Value::String(stub_access.clone()),
            );
            tokens.insert(
                "refresh_token".to_string(),
                serde_json::Value::String(stub_refresh.clone()),
            );
        }
        let stub_json = serde_json::to_string_pretty(&stub_value)
            .map_err(|error| format!("serialize stub auth.json: {error}"))?;

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
        if host == AUTH_OPENAI_HOST && req.uri().path().ends_with(OAUTH_TOKEN_PATH_SUFFIX) {
            return handle_oauth_request(req, server, pending).await;
        }
        // chatgpt.com / chat.openai.com / subdomains all use a Bearer
        // access token in the Authorization header for codex-cli's
        // backend calls.
        handle_bearer_request(req, server).await
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
            *pending = Some(flow);
            return res;
        }

        let (parts, body) = res.into_parts();
        let collected = match body.collect().await {
            Ok(c) => c.to_bytes(),
            Err(error) => {
                eprintln!("pillbox: vault: failed to collect codex oauth response body: {error}");
                return Response::from_parts(parts, Body::empty());
            }
        };

        let mut value: serde_json::Value = match serde_json::from_slice(&collected) {
            Ok(v) => v,
            Err(error) => {
                eprintln!(
                    "pillbox: vault: codex oauth response not JSON; passing through: {error}"
                );
                return Response::from_parts(parts, Body::from(collected));
            }
        };

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
                        "/tokens/access_token",
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
                        "/tokens/refresh_token",
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
}

/// Swap stub refresh_token → real refresh_token on the way out to
/// `auth.openai.com/oauth/token`. Codex sends a JSON body
/// `{client_id, grant_type:"refresh_token", refresh_token}`.
async fn handle_oauth_request(
    req: Request<Body>,
    server: &ServerInner,
    pending: &mut Option<PendingFlow>,
) -> RequestOrResponse {
    let (mut parts, body) = req.into_parts();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(error) => {
            eprintln!("pillbox: vault: failed to collect codex oauth request body: {error}");
            return unauthorized("body read error").into();
        }
    };

    let mut value: serde_json::Value = match serde_json::from_slice(&collected) {
        Ok(v) => v,
        Err(_) => {
            return unauthorized("non-json oauth body").into();
        }
    };

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
            .and_then(|v| v.pointer("/tokens/refresh_token"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    } else {
        None
    };

    if let (Some(obj), Some(real)) = (value.as_object_mut(), real_refresh) {
        obj.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(real),
        );
    }

    let new_body = serde_json::to_vec(&value).unwrap_or(collected.to_vec());
    let new_len = new_body.len();
    parts.headers.remove("content-length");
    parts
        .headers
        .insert("content-length", HeaderValue::from(new_len));
    Request::from_parts(parts, Body::from(new_body)).into()
}

/// Swap stub bearer access_token → real bearer access_token on the way
/// out to chatgpt.com / chat.openai.com.
async fn handle_bearer_request(
    req: Request<Body>,
    server: &ServerInner,
) -> RequestOrResponse {
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

    /// JWT-shaped strings (3 dot-separated parts). We're not validating
    /// the JWT signature — codex's own validator handles that downstream
    /// — but the shape check in `provision` requires it.
    const FAKE_JWT: &str =
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature_part_here";

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
        let access = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap();
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
        let err = CodexProvider.provision("sbx", &bad, &mut registry).unwrap_err();
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
}
