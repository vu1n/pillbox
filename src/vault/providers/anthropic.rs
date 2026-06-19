//! Anthropic provider — Claude Code OAuth.
//!
//! Mirrors the v0.4 single-provider implementation: intercept
//! `api.anthropic.com` (bearer-token swap on every request) and
//! `console.anthropic.com/oauth/token` (refresh-token swap on the way
//! out, real → stub swap + registry rotation on the way back).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use http_body_util::BodyExt;
use hudsucker::{
    hyper::{
        header::{HeaderValue, AUTHORIZATION},
        HeaderMap, Request, Response,
    },
    Body, RequestOrResponse,
};
use serde::Deserialize;

use super::{
    host_from_uri, mint_stub, swap_raw_header, unauthorized, ApiKeySwap, PendingFlow, Registry,
    SandboxData, VaultProvider,
};
use crate::vault::server::ServerInner;
use crate::vault::token_store::{Begin, RefreshDecider, TokenStore};

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

    fn hosts(&self) -> &'static [&'static str] {
        &[API_HOST, CONSOLE_HOST, PLATFORM_HOST]
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
    // Capture the request target before consuming `req`; the coordinated refresh
    // forwards to the guest's own endpoint (a faithful relay, not a reconstruction).
    let host = host_from_uri(&req).unwrap_or_default();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let (parts, body) = req.into_parts();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(error) => {
            eprintln!("pillbox: vault: failed to collect oauth request body: {error}");
            return unauthorized("body read error").into();
        }
    };

    let value: serde_json::Value = match serde_json::from_slice(&collected) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON; we can't rewrite. Reject — refusing is safer
            // than leaking the real token by accident.
            return unauthorized("non-json oauth body").into();
        }
    };

    // Two grant types reach /oauth/token:
    //
    // 1. `grant_type=refresh_token` — the agent's normal token refresh. The body
    //    carries a `refresh_token` that's one of our minted stubs. This is the
    //    reuse-sensitive path: across concurrent sessions sharing the account, only
    //    ONE may forward the real refresh token upstream. Routed through the
    //    `TokenStore` begin/commit coordinator (`coordinate_refresh_request`).
    //
    // 2. `grant_type=authorization_code` — Claude Code's `/login` flow. The body
    //    carries a one-time browser `code`, not a refresh token; it MINTS the
    //    initial token pair (no reuse risk), so it keeps the legacy
    //    forward + `handle_response` path.
    match value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
    {
        Some(stub) => {
            coordinate_refresh_request(host, path_and_query, parts, value, stub, server, pending)
                .await
        }
        None => forward_authorization_code(parts, value, collected.to_vec(), server, pending),
    }
}

/// The `grant_type=authorization_code` path: identify the sandbox, set `pending`
/// so `handle_response` rewrites the freshly-minted real tokens to stubs, and
/// forward the request body verbatim (the code isn't ours to rewrite).
fn forward_authorization_code(
    mut parts: hudsucker::hyper::http::request::Parts,
    value: serde_json::Value,
    original: Vec<u8>,
    server: &ServerInner,
    pending: &mut Option<PendingFlow>,
) -> RequestOrResponse {
    if let Some(sandbox_id) = server
        .registry_lock()
        .unique_sandbox_for_provider(PROVIDER_ID)
    {
        *pending = Some(PendingFlow {
            provider_id: PROVIDER_ID,
            sandbox_id,
        });
    }
    let new_body = serde_json::to_vec(&value).unwrap_or(original);
    force_identity_encoding(&mut parts.headers, new_body.len());
    Request::from_parts(parts, Body::from(new_body)).into()
}

/// `grant_type=refresh_token` — the coordinated in-proxy refresh (slice 2b).
///
/// The vault makes its OWN upstream POST (a faithful relay of the guest's request,
/// only the stub refresh token swapped for the real one) while holding the
/// rotation flock, so across concurrent sessions sharing the account exactly one
/// forward happens and the rest coalesce on its result. Returns a synthesized
/// re-stubbed response directly to the guest — `handle_response` does not run for
/// this path, so `pending` is left unset.
async fn coordinate_refresh_request(
    host: String,
    path_and_query: String,
    mut parts: hudsucker::hyper::http::request::Parts,
    mut value: serde_json::Value,
    stub: String,
    server: &ServerInner,
    pending: &mut Option<PendingFlow>,
) -> RequestOrResponse {
    // Snapshot what the coordinator needs under one registry lock.
    let (sandbox_id, real, creds_path, stub_pair) = {
        let registry = server.registry_lock();
        let Some(sandbox_id) = registry.sandbox_for_stub(&stub).map(str::to_owned) else {
            return unauthorized("unknown stub refresh token").into();
        };
        let real = registry.real(&sandbox_id).cloned();
        let creds_path = registry.host_creds_path(&sandbox_id).map(Path::to_path_buf);
        let stubs = stubs_for(&registry, &sandbox_id);
        (sandbox_id, real, creds_path, stubs)
    };

    let (Some(real), Some((stub_access, stub_refresh))) = (real, stub_pair) else {
        return unauthorized("refresh: missing real creds or stubs").into();
    };

    // No coordination path recorded (an OAuth lease always records one via
    // `set_oauth_creds_path`; this is the bare-lease/test case) → legacy
    // forward+handle_response keeps behavior unchanged.
    let Some(creds_path) = creds_path else {
        if let Some(obj) = value.as_object_mut() {
            if let Some(real_rt) = claude_refresh(&real) {
                obj.insert(
                    "refresh_token".to_string(),
                    serde_json::Value::String(real_rt),
                );
            }
        }
        *pending = Some(PendingFlow {
            provider_id: PROVIDER_ID,
            sandbox_id,
        });
        let new_body = serde_json::to_vec(&value).unwrap_or_default();
        force_identity_encoding(&mut parts.headers, new_body.len());
        return Request::from_parts(parts, Body::from(new_body)).into();
    };

    let decider = ClaudeRefreshDecider {
        mapped_access: claude_access(&real),
    };
    let url = format!("https://{host}{path_and_query}");
    let headers = forwardable_headers(&parts.headers);
    let store = TokenStore::new(creds_path, REFRESH_LOCK_WAIT);

    // begin (blocking flock) + the reqwest::blocking forward + commit all run
    // off-reactor in one hop (reqwest::blocking panics inside a runtime, and the
    // guard holds the lock across the whole forward).
    let outcome = tokio::task::spawn_blocking({
        let (stub_access, stub_refresh) = (stub_access.clone(), stub_refresh.clone());
        move || {
            coordinate_refresh(
                store,
                decider,
                url,
                headers,
                value,
                real,
                stub_access,
                stub_refresh,
            )
        }
    })
    .await;

    let response = match outcome {
        Ok(Coordinated::Committed {
            upstream_body,
            new_real,
        }) => {
            apply_registry_rotation(server, &sandbox_id, &new_real);
            restub_oauth_response(&upstream_body, &stub_access, &stub_refresh)
        }
        Ok(Coordinated::Coalesced { disk }) => {
            apply_registry_rotation(server, &sandbox_id, &disk);
            synth_oauth_response(&disk, &stub_access, &stub_refresh)
        }
        // Upstream rejected/failed the forward (the guard aborted → fail closed).
        // Never relay the upstream body verbatim — it could echo token material;
        // keep only the standard OAuth error fields so the guest still learns it's
        // (e.g.) invalid_grant and re-auths.
        Ok(Coordinated::Forwarded { status, body }) => scrub_oauth_error_response(status, &body),
        Ok(Coordinated::Reauth) => oauth_error_response(
            400,
            "invalid_grant",
            "the subscription token must be refreshed via `pillbox auth login`",
        ),
        Ok(Coordinated::LockBusy) => oauth_error_response(
            503,
            "temporarily_unavailable",
            "a concurrent token refresh is in flight; retry",
        ),
        Ok(Coordinated::Error(detail)) => {
            eprintln!("pillbox: vault: coordinated refresh failed: {detail}");
            oauth_error_response(502, "server_error", "token refresh failed")
        }
        Err(join_err) => {
            eprintln!("pillbox: vault: refresh task panicked: {join_err}");
            oauth_error_response(502, "server_error", "token refresh failed")
        }
    };
    response.into()
}

/// How long to wait for the cross-process rotation flock before giving the guest
/// a retryable 503. Generous: under contention a loser blocks until the winner's
/// forward commits, then coalesces — `LockBusy` only fires if a winner's forward
/// itself wedges past this.
const REFRESH_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Decides the refresh for the claude creds shape. `mapped_access` is the real
/// access token this session is currently mapped to (the registry snapshot); a
/// forward is due only while the on-disk access token still equals it — if a peer
/// already advanced it, [`TokenStore::begin`] coalesces instead.
struct ClaudeRefreshDecider {
    mapped_access: Option<String>,
}

impl RefreshDecider for ClaudeRefreshDecider {
    fn needs_refresh(&self, creds: &serde_json::Value) -> bool {
        match (claude_access(creds), self.mapped_access.as_deref()) {
            (Some(disk), Some(mapped)) => disk == mapped,
            // Unknown shape / no baseline → let begin attempt; it fails closed to
            // re-auth if the refresh/access tokens aren't actually present.
            _ => true,
        }
    }
    fn refresh_token(&self, creds: &serde_json::Value) -> Option<String> {
        claude_refresh(creds)
    }
    fn access_token(&self, creds: &serde_json::Value) -> Option<String> {
        claude_access(creds)
    }
}

/// Outcome of the off-reactor coordinator, mapped back to a guest response.
enum Coordinated {
    /// This caller won the race and committed the rotation. `upstream_body` is the
    /// 2xx token response (to re-stub); `new_real` is the persisted real creds.
    Committed {
        upstream_body: Vec<u8>,
        new_real: serde_json::Value,
    },
    /// A peer already rotated; adopt these on-disk creds (synthesize a response).
    Coalesced { disk: serde_json::Value },
    /// The upstream forward returned non-2xx (or an unparseable 2xx); the guard
    /// aborted (fail closed). Relay the upstream status + body to the guest.
    Forwarded { status: u16, body: Vec<u8> },
    /// A prior refresh's outcome is unknown, or the tokens are missing → re-auth.
    Reauth,
    /// The rotation lock couldn't be acquired in time → retryable.
    LockBusy,
    /// Transport/serialize/commit error; the guard aborted (fail closed).
    Error(String),
}

/// The blocking half: `begin` → forward the guest's request with the real refresh
/// token → `commit`/`abort`. Runs inside `spawn_blocking` (flock + reqwest::blocking
/// both block, and the guard holds the lock across the whole forward).
#[allow(clippy::too_many_arguments)]
fn coordinate_refresh(
    store: TokenStore,
    decider: ClaudeRefreshDecider,
    url: String,
    headers: reqwest::header::HeaderMap,
    mut body: serde_json::Value,
    real_snapshot: serde_json::Value,
    stub_access: String,
    stub_refresh: String,
) -> Coordinated {
    let guard = match store.begin(&decider) {
        Ok(Begin::Rotate(g)) => g,
        Ok(Begin::Coalesced(disk)) => return Coordinated::Coalesced { disk },
        Ok(Begin::ReauthRequired(_)) => return Coordinated::Reauth,
        Ok(Begin::LockBusy) => return Coordinated::LockBusy,
        Err(e) => return Coordinated::Error(format!("begin: {e}")),
    };

    // Swap the stub refresh token for the real one read from disk under the lock.
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(guard.refresh_token().to_string()),
        );
    }
    let body_bytes = match serde_json::to_vec(&body) {
        Ok(b) => b,
        Err(e) => {
            guard.abort();
            return Coordinated::Error(format!("serialize body: {e}"));
        }
    };

    // `.no_proxy()` is load-bearing: the vault's own forward must reach the REAL
    // upstream directly. reqwest otherwise auto-reads HTTPS_PROXY from the host env,
    // which — if set (the user's shell, or inherited) — routes this POST back through
    // the vault proxy. The loop hands back a re-stubbed response, and committing that
    // stub clobbers the real creds file with a stub. Forward direct, always.
    let client = match reqwest::blocking::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            guard.abort();
            return Coordinated::Error(format!("build client: {e}"));
        }
    };
    let resp = match client.post(&url).headers(headers).body(body_bytes).send() {
        Ok(r) => r,
        Err(e) => {
            guard.abort();
            return Coordinated::Error(format!("forward: {e}"));
        }
    };
    let status = resp.status().as_u16();
    let resp_body = match resp.bytes() {
        Ok(b) => b.to_vec(),
        Err(e) => {
            guard.abort();
            return Coordinated::Error(format!("read response: {e}"));
        }
    };
    // A refresh is rare (once per token lifetime); log where the vault's own
    // upstream call went and how it landed — URL + status only, no token material.
    // Cheap confirmation that the forward reached Anthropic directly (not a loop).
    eprintln!("pillbox: vault: refresh forward {url} → HTTP {status}");

    if !(200..300).contains(&status) {
        // Upstream rejected the refresh → fail closed (the next begin re-auths).
        guard.abort();
        return Coordinated::Forwarded {
            status,
            body: resp_body,
        };
    }

    let upstream: serde_json::Value = match serde_json::from_slice(&resp_body) {
        Ok(v) => v,
        Err(_) => {
            guard.abort();
            return Coordinated::Forwarded {
                status,
                body: resp_body,
            };
        }
    };

    // Loop guard (defense in depth, independent of `.no_proxy()`): if the "upstream"
    // handed back one of THIS session's own stubs, the forward looped through the
    // vault instead of reaching Anthropic. Committing that stub would clobber the
    // real creds file. Fail closed rather than persist a stub.
    if is_stub_loopback(&upstream, &stub_access, &stub_refresh) {
        guard.abort();
        eprintln!(
            "pillbox: vault: refresh forward to {url} looped back (got our own stub); \
             refusing to commit — an HTTPS_PROXY in the environment is routing the \
             vault's own upstream call back through itself"
        );
        return Coordinated::Error("refresh forward looped back through the vault".into());
    }

    let Some(new_real) = build_new_real(&real_snapshot, &upstream) else {
        // 2xx without a usable access token — don't clear pending, relay as-is.
        guard.abort();
        return Coordinated::Forwarded {
            status,
            body: resp_body,
        };
    };

    if let Err(e) = guard.commit(&decider, new_real.clone()) {
        // commit refused (access didn't rotate) or write failed → pending stays
        // set (fail closed). The guest sees a server error and re-auths.
        return Coordinated::Error(format!("commit: {e}"));
    }
    Coordinated::Committed {
        upstream_body: resp_body,
        new_real,
    }
}

/// A pillbox-minted stub by SHAPE: `mint_stub` emits `<prefix>` + three
/// `uuid_v7().simple()` runs = a 96-char pure-lowercase-hex tail. Real Anthropic
/// tokens carry mixed-case/non-hex tails, so this is an unambiguous structural
/// tell — and catches a looped stub from ANY sandbox, not just this session's.
fn is_stub_shaped(token: &str) -> bool {
    token
        .strip_prefix(STUB_ACCESS_PREFIX)
        .or_else(|| token.strip_prefix(STUB_REFRESH_PREFIX))
        .is_some_and(|tail| {
            tail.len() == 96
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// True if the "upstream" token response handed back a pillbox stub — the signature
/// of a forward that looped through the vault instead of reaching the real endpoint.
/// Committing such a response would clobber the real creds file with a stub, so the
/// coordinator fails closed when this holds. Checks both this session's exact stubs
/// AND the structural stub shape (a loop can surface another sandbox's stub).
fn is_stub_loopback(upstream: &serde_json::Value, stub_access: &str, stub_refresh: &str) -> bool {
    let acc = upstream.get("access_token").and_then(|v| v.as_str());
    let refr = upstream.get("refresh_token").and_then(|v| v.as_str());
    acc == Some(stub_access)
        || refr == Some(stub_refresh)
        || acc.is_some_and(is_stub_shaped)
        || refr.is_some_and(is_stub_shaped)
}

/// Splice the upstream token response into the real creds (claude shape). `None`
/// if the response carries no access token (not a usable rotation).
fn build_new_real(
    real_snapshot: &serde_json::Value,
    upstream: &serde_json::Value,
) -> Option<serde_json::Value> {
    let new_access = upstream.get("access_token").and_then(|v| v.as_str())?;
    let mut new_real = real_snapshot.clone();
    let oauth = new_real.get_mut("claudeAiOauth")?.as_object_mut()?;
    oauth.insert(
        "accessToken".to_string(),
        serde_json::Value::String(new_access.to_string()),
    );
    if let Some(new_refresh) = upstream.get("refresh_token").and_then(|v| v.as_str()) {
        oauth.insert(
            "refreshToken".to_string(),
            serde_json::Value::String(new_refresh.to_string()),
        );
    }
    if let Some(expires_in) = upstream.get("expires_in").and_then(|v| v.as_u64()) {
        oauth.insert(
            "expiresAt".to_string(),
            serde_json::Value::from(now_ms().saturating_add(expires_in.saturating_mul(1000))),
        );
    }
    Some(new_real)
}

/// Mirror the committed real tokens into the in-memory registry so this session's
/// subsequent bearer-token API swaps map the (stable) stub to the new real access
/// token. The file is the cross-process authority; the registry is the in-session
/// swap map. Called the instant the off-reactor commit returns, before the guest
/// gets its refresh response. A bearer call the guest fires concurrently in the
/// sub-millisecond window between `commit` and here swaps the old access token and
/// gets a transient 401, which the agent's own retry resolves — tolerated rather
/// than holding the registry lock across the whole forward.
fn apply_registry_rotation(server: &ServerInner, sandbox_id: &str, new_real: &serde_json::Value) {
    let mut registry = server.registry_lock();
    if let Some(access) = claude_access(new_real) {
        registry.rotate_real_field(sandbox_id, "/claudeAiOauth/accessToken", access);
    }
    if let Some(refresh) = claude_refresh(new_real) {
        registry.rotate_real_field(sandbox_id, "/claudeAiOauth/refreshToken", refresh);
    }
}

/// Re-stub the upstream 2xx token body: swap the new real tokens back to the
/// session's stable stubs so the guest never sees a real bearer.
fn restub_oauth_response(
    upstream_body: &[u8],
    stub_access: &str,
    stub_refresh: &str,
) -> Response<Body> {
    let mut value: serde_json::Value = serde_json::from_slice(upstream_body).unwrap_or_default();
    if let Some(obj) = value.as_object_mut() {
        if obj.contains_key("access_token") {
            obj.insert(
                "access_token".to_string(),
                serde_json::Value::String(stub_access.to_string()),
            );
        }
        if obj.contains_key("refresh_token") {
            obj.insert(
                "refresh_token".to_string(),
                serde_json::Value::String(stub_refresh.to_string()),
            );
        }
    }
    json_response(200, &value)
}

/// Synthesize a token response for the coalesce path — a peer already rotated, so
/// hand the guest its (stable) stubs with the freshened expiry, no upstream call.
fn synth_oauth_response(
    disk: &serde_json::Value,
    stub_access: &str,
    stub_refresh: &str,
) -> Response<Body> {
    let expires_in = disk
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(|v| v.as_u64())
        .map(|exp_ms| exp_ms.saturating_sub(now_ms()) / 1000)
        .unwrap_or(3600);
    let body = serde_json::json!({
        "access_token": stub_access,
        "refresh_token": stub_refresh,
        "expires_in": expires_in,
        "token_type": "Bearer",
    });
    json_response(200, &body)
}

fn claude_access(creds: &serde_json::Value) -> Option<String> {
    creds
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

fn claude_refresh(creds: &serde_json::Value) -> Option<String> {
    creds
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Copy the guest's headers for the upstream relay, dropping the ones reqwest must
/// recompute (`host`, `content-length`) and forcing identity encoding (reqwest's
/// blocking client has no gzip feature here, so it would not decompress a gzipped
/// token response). Reconstructed via bytes so it doesn't assume hudsucker's and
/// reqwest's header types are the same.
fn forwardable_headers(src: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in src.iter() {
        let n = name.as_str().to_ascii_lowercase();
        if n == "host" || n == "content-length" || n == "accept-encoding" {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(hn, hv);
        }
    }
    out.insert(
        reqwest::header::ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    );
    out
}

/// Force the forwarded request body back uncompressed + set the rewritten length.
/// Anthropic gzips `/oauth/token` responses when the client advertises gzip (Claude
/// Code does); identity keeps the response parseable on the way back.
fn force_identity_encoding(headers: &mut HeaderMap, content_length: usize) {
    headers.remove("content-length");
    headers.insert("content-length", HeaderValue::from(content_length));
    headers.remove("accept-encoding");
    headers.insert("accept-encoding", HeaderValue::from_static("identity"));
}

fn json_response(status: u16, value: &serde_json::Value) -> Response<Body> {
    raw_response(status, serde_json::to_vec(value).unwrap_or_default())
}

fn raw_response(status: u16, body: Vec<u8>) -> Response<Body> {
    let len = body.len();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("content-length", len)
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn oauth_error_response(status: u16, error: &str, description: &str) -> Response<Body> {
    json_response(
        status,
        &serde_json::json!({ "error": error, "error_description": description }),
    )
}

/// Build a guest response from an upstream OAuth error WITHOUT relaying its body
/// verbatim — an upstream error body could echo token material. Keep only the
/// standard, tokenless OAuth error fields (`error` / `error_description`); a
/// non-JSON or fieldless body collapses to a generic `server_error`.
fn scrub_oauth_error_response(status: u16, body: &[u8]) -> Response<Body> {
    let scrubbed = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let error = v.get("error").and_then(|e| e.as_str())?.to_string();
            let description = v
                .get("error_description")
                .and_then(|d| d.as_str())
                .unwrap_or("token refresh failed")
                .to_string();
            Some(serde_json::json!({ "error": error, "error_description": description }))
        })
        .unwrap_or_else(|| serde_json::json!({ "error": "server_error" }));
    json_response(status, &scrubbed)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

    // ── Coordinated refresh (slice 2b) ──────────────────────────────────────
    //
    // The Rotate → upstream forward → commit path needs a live endpoint (covered
    // by live smoke). These exercise everything around it: the decider's coalesce
    // gate, the creds-shaping helpers, and the non-forward `begin` branches
    // (Coalesced / Reauth) the coordinator selects WITHOUT touching the network.

    use crate::vault::token_store::TokenStore;

    fn future_expiry_ms() -> u64 {
        now_ms() + 3_600_000
    }

    #[test]
    fn decider_gates_refresh_on_access_token_equality() {
        let d = ClaudeRefreshDecider {
            mapped_access: Some("AT0".into()),
        };
        // disk still on the access we mapped → a forward is due.
        assert!(d.needs_refresh(&serde_json::json!({ "claudeAiOauth": { "accessToken": "AT0" } })));
        // a peer already advanced disk → coalesce, don't forward.
        assert!(!d.needs_refresh(&serde_json::json!({ "claudeAiOauth": { "accessToken": "AT1" } })));
        assert_eq!(
            d.access_token(&serde_json::json!({ "claudeAiOauth": { "accessToken": "AT0" } }))
                .as_deref(),
            Some("AT0")
        );
        assert_eq!(
            d.refresh_token(&serde_json::json!({ "claudeAiOauth": { "refreshToken": "RT0" } }))
                .as_deref(),
            Some("RT0")
        );
    }

    #[test]
    fn build_new_real_splices_tokens_and_expiry_preserving_other_fields() {
        let real = sample_anthropic_real();
        let upstream = serde_json::json!({
            "access_token": "NEW_A", "refresh_token": "NEW_R", "expires_in": 3600,
        });
        let nr = build_new_real(&real, &upstream).expect("usable rotation");
        assert_eq!(nr.pointer("/claudeAiOauth/accessToken").unwrap(), "NEW_A");
        assert_eq!(nr.pointer("/claudeAiOauth/refreshToken").unwrap(), "NEW_R");
        assert!(
            nr.pointer("/claudeAiOauth/expiresAt")
                .and_then(|v| v.as_u64())
                .unwrap()
                > now_ms()
        );
        // Untouched fields survive the splice.
        assert_eq!(
            nr.pointer("/claudeAiOauth/subscriptionType").unwrap(),
            "pro"
        );
    }

    #[test]
    fn build_new_real_none_without_access_token() {
        // A 2xx body that isn't a usable token response → no rotation to commit.
        assert!(build_new_real(
            &sample_anthropic_real(),
            &serde_json::json!({ "token_type": "Bearer" })
        )
        .is_none());
    }

    #[tokio::test]
    async fn scrub_oauth_error_drops_token_fields_keeps_error() {
        // An upstream error body that echoes token material must NOT reach the
        // guest verbatim — only the standard OAuth error fields survive.
        let body = serde_json::to_vec(&serde_json::json!({
            "error": "invalid_grant",
            "error_description": "refresh token expired",
            "access_token": "LEAKED_REAL_ACCESS",
            "refresh_token": "LEAKED_REAL_REFRESH",
        }))
        .unwrap();
        let resp = scrub_oauth_error_response(400, &body);
        assert_eq!(resp.status(), 400);
        let v = body_json(resp.into_body()).await;
        assert_eq!(v["error"], "invalid_grant");
        assert_eq!(v["error_description"], "refresh token expired");
        assert!(v.get("access_token").is_none(), "token fields scrubbed");
        assert!(v.get("refresh_token").is_none(), "token fields scrubbed");
    }

    #[tokio::test]
    async fn scrub_oauth_error_collapses_non_json_to_generic() {
        let resp = scrub_oauth_error_response(502, b"<html>gateway error sk-ant-oat01-leak</html>");
        let v = body_json(resp.into_body()).await;
        assert_eq!(v["error"], "server_error");
        // The raw (potentially token-bearing) body never reaches the guest.
        assert!(!v.to_string().contains("sk-ant"));
    }

    #[tokio::test]
    async fn restub_oauth_response_swaps_real_tokens_back_to_stubs() {
        let upstream_body = serde_json::to_vec(&serde_json::json!({
            "access_token": "NEW_REAL_A", "refresh_token": "NEW_REAL_R", "expires_in": 3600,
        }))
        .unwrap();
        let resp = restub_oauth_response(&upstream_body, "sk-ant-oat01-stub", "sk-ant-ort01-stub");
        assert_eq!(resp.status(), 200);
        let v = body_json(resp.into_body()).await;
        assert_eq!(v["access_token"], "sk-ant-oat01-stub");
        assert_eq!(v["refresh_token"], "sk-ant-ort01-stub");
        // The real tokens never reach the guest.
        assert_ne!(v["access_token"], "NEW_REAL_A");
    }

    #[tokio::test]
    async fn synth_oauth_response_carries_stubs_and_a_positive_expiry() {
        let disk = serde_json::json!({
            "claudeAiOauth": { "accessToken": "AT", "refreshToken": "RT", "expiresAt": future_expiry_ms() },
        });
        let resp = synth_oauth_response(&disk, "stubA", "stubR");
        assert_eq!(resp.status(), 200);
        let v = body_json(resp.into_body()).await;
        assert_eq!(v["access_token"], "stubA");
        assert_eq!(v["refresh_token"], "stubR");
        let expires_in = v["expires_in"].as_u64().unwrap();
        assert!(expires_in > 0 && expires_in <= 3600, "got {expires_in}");
    }

    fn store_on(creds: serde_json::Value) -> (tempfile::TempDir, TokenStore) {
        let dir = tempfile::tempdir().unwrap();
        let creds_path = dir.path().join(".credentials.json");
        std::fs::write(&creds_path, serde_json::to_vec(&creds).unwrap()).unwrap();
        let store = TokenStore::new(creds_path, std::time::Duration::from_secs(5));
        (dir, store)
    }

    #[test]
    fn coordinate_refresh_coalesces_when_disk_advanced_no_network() {
        // Disk is on AT1; this session mapped AT0 → a peer already rotated →
        // begin() returns Coalesced and the coordinator never forwards.
        let (_d, store) = store_on(serde_json::json!({
            "claudeAiOauth": { "accessToken": "AT1", "refreshToken": "RT1", "expiresAt": future_expiry_ms() },
        }));
        let decider = ClaudeRefreshDecider {
            mapped_access: Some("AT0".into()),
        };
        let out = coordinate_refresh(
            store,
            decider,
            "https://unused.invalid/v1/oauth/token".into(),
            reqwest::header::HeaderMap::new(),
            serde_json::json!({ "grant_type": "refresh_token", "refresh_token": "stub" }),
            sample_anthropic_real(),
            "sk-ant-oat01-stubA".into(),
            "sk-ant-ort01-stubR".into(),
        );
        match out {
            Coordinated::Coalesced { disk } => {
                assert_eq!(disk.pointer("/claudeAiOauth/accessToken").unwrap(), "AT1");
            }
            _ => panic!("expected Coalesced"),
        }
    }

    #[test]
    fn coordinate_refresh_reauths_when_refresh_token_missing_no_network() {
        // Disk has the mapped access token (so a forward is "due") but no refresh
        // token → begin() fails closed to re-auth; the coordinator never forwards.
        let (_d, store) = store_on(serde_json::json!({
            "claudeAiOauth": { "accessToken": "AT0" },
        }));
        let decider = ClaudeRefreshDecider {
            mapped_access: Some("AT0".into()),
        };
        let out = coordinate_refresh(
            store,
            decider,
            "https://unused.invalid/v1/oauth/token".into(),
            reqwest::header::HeaderMap::new(),
            serde_json::json!({ "refresh_token": "stub" }),
            sample_anthropic_real(),
            "sk-ant-oat01-stubA".into(),
            "sk-ant-ort01-stubR".into(),
        );
        assert!(matches!(out, Coordinated::Reauth));
    }

    #[test]
    fn is_stub_loopback_detects_our_own_stubs() {
        // Regression for the live-smoke clobber: a proxy loop hands back THIS
        // session's own stubs as if they were freshly minted real tokens. The guard
        // must catch that so the coordinator fails closed instead of committing a
        // stub over the real creds file.
        let (stub_a, stub_r) = ("sk-ant-oat01-mineA", "sk-ant-ort01-mineR");
        // A looped response (vault re-stubbed with our stubs) → detected.
        assert!(is_stub_loopback(
            &serde_json::json!({ "access_token": stub_a, "refresh_token": stub_r }),
            stub_a,
            stub_r
        ));
        // Either field matching is enough (Anthropic doesn't always rotate the RT).
        assert!(is_stub_loopback(
            &serde_json::json!({ "access_token": stub_a, "refresh_token": "REAL_R" }),
            stub_a,
            stub_r
        ));
        // A genuine real upstream response (different tokens) → not a loop.
        assert!(!is_stub_loopback(
            &serde_json::json!({ "access_token": "REAL_A", "refresh_token": "REAL_R" }),
            stub_a,
            stub_r
        ));
        // A loop can surface ANOTHER sandbox's stub (not our exact pair) — the
        // structural check still catches it. This is the case the exact-match guard
        // missed in the live smoke.
        let other = format!("sk-ant-oat01-{}", "0123456789abcdef".repeat(6)); // 96 hex
        assert_eq!(other.len(), 109);
        assert!(is_stub_shaped(&other));
        assert!(is_stub_loopback(
            &serde_json::json!({ "access_token": other, "refresh_token": "REAL_R" }),
            stub_a,
            stub_r
        ));
        // A real token (mixed case / non-hex tail) is not stub-shaped.
        assert!(!is_stub_shaped(
            "sk-ant-oat01-52xRZAGfjjK9someRealMixedCaseTail"
        ));
    }

    /// One-shot local HTTP server: accepts a single connection, drains the request,
    /// and replies with `status` + `body`. Stands in for the real `/oauth/token`
    /// endpoint so the full forward→commit path is exercised without claude or the
    /// network — the piece the live smoke couldn't pin down deterministically.
    fn spawn_oauth_server(status: u16, body: serde_json::Value) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = serde_json::to_vec(&body).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf); // drain request headers+body (small)
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}/v1/oauth/token")
    }

    fn expired_real_disk() -> serde_json::Value {
        serde_json::json!({
            "claudeAiOauth": { "accessToken": "AT0", "refreshToken": "RT0", "expiresAt": 1 },
        })
    }

    #[test]
    fn coordinate_refresh_commits_real_tokens_on_success() {
        // The success path: a real upstream 200 → commit the REAL rotated tokens to
        // the creds file (NOT a stub). This is the path the live smoke kept routing
        // around (legacy fallback) or failing early (consumed RT / env-proxy loop).
        let url = spawn_oauth_server(
            200,
            serde_json::json!({
                "access_token": "REAL_NEW_ACCESS",
                "refresh_token": "REAL_NEW_REFRESH",
                "expires_in": 3600,
            }),
        );
        let (dir, store) = store_on(expired_real_disk());
        let decider = ClaudeRefreshDecider {
            mapped_access: Some("AT0".into()),
        };
        let out = coordinate_refresh(
            store,
            decider,
            url,
            reqwest::header::HeaderMap::new(),
            serde_json::json!({ "grant_type": "refresh_token", "refresh_token": "sk-ant-ort01-stub" }),
            sample_anthropic_real(),
            "sk-ant-oat01-myStub".into(),
            "sk-ant-ort01-myStub".into(),
        );
        assert!(
            matches!(out, Coordinated::Committed { .. }),
            "expected a real commit"
        );
        // The creds file holds the REAL rotated tokens — not a stub.
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(".credentials.json")).unwrap())
                .unwrap();
        assert_eq!(
            disk.pointer("/claudeAiOauth/accessToken").unwrap(),
            "REAL_NEW_ACCESS"
        );
        assert_eq!(
            disk.pointer("/claudeAiOauth/refreshToken").unwrap(),
            "REAL_NEW_REFRESH"
        );
        // Real metadata preserved from the snapshot.
        assert_eq!(
            disk.pointer("/claudeAiOauth/subscriptionType").unwrap(),
            "pro"
        );
    }

    #[test]
    fn coordinate_refresh_refuses_a_stub_response_and_leaves_creds_untouched() {
        // The clobber-prevention path: if the "upstream" 200 hands back a stub (the
        // loop signature), the loop-guard fails closed — NO commit, creds file
        // untouched. This is the exact corruption the live smoke produced.
        let stub = format!("sk-ant-oat01-{}", "0123456789abcdef".repeat(6)); // 96 hex
        let url = spawn_oauth_server(
            200,
            serde_json::json!({
                "access_token": stub,
                "refresh_token": format!("sk-ant-ort01-{}", "0123456789abcdef".repeat(6)),
                "expires_in": 3600,
            }),
        );
        let (dir, store) = store_on(expired_real_disk());
        let decider = ClaudeRefreshDecider {
            mapped_access: Some("AT0".into()),
        };
        let out = coordinate_refresh(
            store,
            decider,
            url,
            reqwest::header::HeaderMap::new(),
            serde_json::json!({ "grant_type": "refresh_token", "refresh_token": "sk-ant-ort01-stub" }),
            sample_anthropic_real(),
            "sk-ant-oat01-myStub".into(),
            "sk-ant-ort01-myStub".into(),
        );
        assert!(
            matches!(out, Coordinated::Error(ref e) if e.contains("looped")),
            "expected the loop-guard to fail closed"
        );
        // Creds file UNCHANGED — the original disk tokens survive, no stub written.
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(".credentials.json")).unwrap())
                .unwrap();
        assert_eq!(disk.pointer("/claudeAiOauth/accessToken").unwrap(), "AT0");
        assert_eq!(disk.pointer("/claudeAiOauth/refreshToken").unwrap(), "RT0");
    }
}
