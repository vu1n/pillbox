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
    host_from_uri, mint_stub, unauthorized, PendingFlow, Registry, SandboxData, VaultProvider,
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
        host == API_HOST || host == CONSOLE_HOST
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
        if host == CONSOLE_HOST && req.uri().path().ends_with(OAUTH_TOKEN_PATH_SUFFIX) {
            return handle_oauth_request(req, server, pending).await;
        }
        if host == API_HOST {
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

async fn handle_api_request(
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
    fn intercept_matches_two_hosts_only() {
        let p = AnthropicProvider;
        assert!(p.intercept("api.anthropic.com"));
        assert!(p.intercept("console.anthropic.com"));
        assert!(!p.intercept("anthropic.com"));
        assert!(!p.intercept("chatgpt.com"));
    }

    #[test]
    fn creds_path_is_claude_credentials() {
        assert_eq!(
            AnthropicProvider.creds_path(),
            Path::new(".claude/.credentials.json")
        );
    }
}
