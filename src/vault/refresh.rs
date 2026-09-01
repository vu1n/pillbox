//! Host-side OAuth token refresh — the Claude and Codex broker.
//!
//! In the broker model the sandbox's stub credentials carry a far-future
//! expiry (`expiresAt` for Claude, a far-future JWT `exp` for Codex), so the
//! agent **never refreshes itself**. All rotation happens here, on the host, at
//! vault session start (`provision_oauth_mount`) — **before** the proxy is up —
//! so the access token the MITM injects on the wire is always fresh. This mirrors
//! what the agents do on a developer's laptop, except pillbox owns the refresh.
//!
//! Routes the refresh through the provider-agnostic
//! [`super::token_store::TokenStore`] single-writer core, so concurrent launches
//! that share one subscription (`dispatch -k`, overlapping `--detach`) rotate the
//! shared refresh token **at most once** instead of each POSTing it and tripping
//! Anthropic's refresh-token-reuse revoke. The store owns the cross-process lock,
//! the at-most-once `pending` discipline, and the atomic write. Provider codecs own
//! token fields, expiry, request construction, and response application; this file
//! owns only coordinated transport. The guest never receives a real refresh
//! credential or a refresh response containing real tokens.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;

use crate::errors::PillboxError;

use super::providers::{self, OAuthCodec};
use super::token_store::{Begin, RotateGuard, TokenStore};

/// Upper bound (in ms) below which a unix timestamp is *certainly* in seconds. 1e11
/// ms is the year 5138 — no real seconds-encoded timestamp will ever exceed this,
/// so anything smaller is safely scaled to ms.
const SECONDS_BOUNDARY_MS: u64 = 100_000_000_000;

/// Refresh tokens this far before their actual `expiresAt` so we don't race a
/// near-expiry token against the request's round-trip latency.
const PRE_EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

/// HTTP timeout for the upstream refresh call. Provider OAuth endpoints typically
/// respond in <1s; 30s buys headroom for a degraded edge without stalling pillbox
/// startup forever.
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`pre_refresh`] waits for the rotation lock before failing closed. A
/// peer holding it is mid-POST of the shared refresh token; out-waiting it (then
/// re-reading the now-rotated creds) is the coalesce. Generous because the held
/// region is a single network round-trip; if a holder stays wedged past this, this
/// waiter's own token is stale too, so [`Begin::LockBusy`] → fail closed is right —
/// and a waiter that gives up POSTs nothing, so it never risks a double-send.
const PRE_REFRESH_LOCK_WAIT: Duration = Duration::from_secs(35);

/// `expiresAt` (unix ms) the broker stamps into the stub creds the guest sees:
/// 2100-01-01, the same far-future sentinel centaur/iron-proxy use. The guest's
/// agent trusts its local expiry and so **never refreshes itself** — the broker move
/// that dissolves the host-creds clobber and the refresh-token-reuse race. The
/// host-side Anthropic proxy and the libkrun env-fork both encode this same instant
/// in their provider-specific stub shapes. The *real* creds keep their true expiry;
/// only the stub copy is post-dated.
pub(crate) const STUB_FAR_FUTURE_EXPIRES_AT_MS: u64 = 4_102_444_800_000;

/// Establish a fresh OAuth credential for `agent_id`, coordinated across concurrent
/// sessions. Called from `provision_oauth_mount` at the start of every vaulted run.
///
/// - `Ok(Some(creds))` — fresh creds to lease (already fresh and coalesced, or
///   rotated this call). On rotation the new pair was persisted to disk atomically
///   under the lock before returning.
/// - `Ok(None)` — `agent_id` has no broker decider → caller leases
///   the on-disk creds unchanged.
/// - `Err` — **fails closed.** The only safe credential to lease in the broker model
///   is a fresh one: the agent won't self-refresh (far-future stub expiry), so a
///   stale access token would simply 401 with no recovery. Surfaces a retry/re-auth
///   next-step rather than handing the run a doomed token.
// Context: doc://pillbox/adr-004-vault-broker-oauth@0001#vault-broker-oauth
pub(crate) fn pre_refresh(creds_path: &Path, agent_id: &str) -> Result<Option<Value>> {
    let Some(provider) = providers::provider_for(agent_id) else {
        return Ok(None);
    };
    let Some(codec) = provider.oauth_codec() else {
        return Ok(None);
    };
    let store = TokenStore::new(creds_path.to_path_buf(), PRE_REFRESH_LOCK_WAIT);
    match store.begin(codec)? {
        Begin::Coalesced(creds) => Ok(Some(creds)),
        Begin::DegradedLease(creds) => {
            eprintln!(
                "warning: serving {agent_id} on a degraded lease: a prior refresh did not commit; \
                 re-auth before the token expires"
            );
            Ok(Some(creds))
        }
        Begin::Rotate(guard) => rotate(guard, codec, agent_id).map(Some),
        Begin::ReauthRequired(reason) => Err(PillboxError::runtime(
            "vault",
            format!("could not establish a fresh OAuth token for `{agent_id}`: {reason}"),
        )
        // The cause may be transient (a connect blip, token intact) or terminal (a
        // revoked family) — the reason distinguishes, but the remedy that covers both
        // is "re-run, then re-auth if it persists". A bare "login" would mislead on
        // the common network-blip case.
        .with_next(format!(
            "retry the command; if it persists, run `pillbox auth login --agent {agent_id}`"
        ))
        .into()),
        Begin::LockBusy => Err(PillboxError::runtime(
            "vault",
            format!(
                "another session is refreshing `{agent_id}` credentials and did not release the \
                 rotation lock within {PRE_REFRESH_LOCK_WAIT:?}"
            ),
        )
        .with_next("retry in a moment".to_string())
        .into()),
    }
}

/// JIT broker refresh for the libkrun in-VMM MITM. Runs the coordinated, at-most-once
/// [`pre_refresh`] (rotate the shared token, or coalesce onto a peer's rotation), then
/// reads the now-current real access token + its expiry back out. The MITM child calls
/// this near expiry, off its poll loop, and splices the returned token into the live
/// swap — so the wire token stays fresh for arbitrarily long sessions without the guest
/// ever refreshing. Reuses `pre_refresh` verbatim, so the single-writer / at-most-once /
/// fail-closed invariants are identical to the start-of-run broker refresh; this only
/// adds the read-back of what `pre_refresh` already established.
#[cfg(feature = "libkrun")]
pub(crate) fn broker_jit_refresh(creds_path: &Path, agent_id: &str) -> Result<(String, u64)> {
    let creds = pre_refresh(creds_path, agent_id)?.ok_or_else(|| {
        // Reached only if a non-broker agent_id were scheduled — but the caller gates on
        // `broker_expiry`, which is None for those. Fail closed rather than guess.
        PillboxError::runtime(
            "vault",
            format!("no broker refresh decider for `{agent_id}`"),
        )
    })?;
    let provider = providers::provider_for(agent_id).ok_or_else(|| {
        PillboxError::runtime(
            "vault",
            format!("no broker refresh decider for `{agent_id}`"),
        )
    })?;
    let codec = provider.oauth_codec().ok_or_else(|| {
        PillboxError::runtime(
            "vault",
            format!("no broker refresh decider for `{agent_id}`"),
        )
    })?;
    let access = codec
        .access_token(&creds)
        .ok_or_else(|| PillboxError::runtime("vault", "refreshed creds missing access token"))?;
    let expires_at_ms = codec
        .expiry_ms(&creds)
        .ok_or_else(|| PillboxError::runtime("vault", "refreshed creds missing expiry"))?;
    Ok((access, expires_at_ms))
}

/// The real access-token expiry (unix ms, seconds-normalized) the broker JIT driver
/// schedules against, or `None` for a non-broker agent (disables JIT) or creds without
/// a usable expiry. Reads the LIVE creds file — the guest sees a post-dated stub copy,
/// but the host file keeps the true expiry the rotation must track.
#[cfg(feature = "libkrun")]
pub(crate) fn broker_expiry(creds_path: &Path, agent_id: &str) -> Option<u64> {
    let provider = providers::provider_for(agent_id)?;
    let codec = provider.oauth_codec()?;
    let text = std::fs::read_to_string(creds_path).ok()?;
    let creds: Value = serde_json::from_str(&text).ok()?;
    codec.expiry_ms(&creds)
}

/// We won the race: POST the refresh grant exactly once, then resolve the guard.
/// The branch in [`post_refresh`] that we reach decides which resolution the
/// at-most-once invariant permits.
fn rotate(guard: RotateGuard, codec: &dyn OAuthCodec, agent_id: &str) -> Result<Value> {
    let refresh = guard.refresh_token().to_owned();
    match post_refresh(codec, &refresh) {
        Ok(resp) => {
            let mut new_creds = guard.base_creds().clone();
            if let Err(e) =
                codec.apply_refresh_response(&mut new_creds, &resp, &refresh, unix_now_ms())
            {
                // A 2xx came back, so a token was almost certainly issued and the old
                // one consumed — but we can't assemble usable creds from the body.
                // Fail closed (leave `pending` set → next run re-auths); the issued
                // token is lost, never re-send the consumed one.
                guard.abort();
                return Err(PillboxError::runtime(
                    "vault",
                    format!("refresh succeeded but its response was unusable: {e}"),
                )
                .with_next(format!("run `pillbox auth login --agent {agent_id}`"))
                .into());
            }
            // `commit` re-checks the access token actually rotated; if not, it returns
            // Err and (via the dropped guard) leaves `pending` set → next run re-auths.
            guard.commit(codec, new_creds.clone())?;
            Ok(new_creds)
        }
        Err(RotateError::Definite(reason)) => {
            // The token provably never went on the wire, or was rejected without being
            // issued: clear `pending` so the next run may retry the intact token.
            guard.abort_intact()?;
            Err(PillboxError::runtime(
                "vault",
                format!("OAuth refresh for `{agent_id}` failed: {reason}"),
            )
            .with_next(format!(
                "retry the command; if it persists, run `pillbox auth login --agent {agent_id}`"
            ))
            .into())
        }
        Err(RotateError::Ambiguous(reason)) => {
            // The POST may have reached the server and consumed the token: leave
            // `pending` set (fail closed) so no peer re-sends it.
            guard.abort();
            Err(PillboxError::runtime(
                "vault",
                format!("OAuth refresh for `{agent_id}` failed: {reason}"),
            )
            .with_next(format!("run `pillbox auth login --agent {agent_id}`"))
            .into())
        }
    }
}

/// Has `expires_at` passed [`PRE_EXPIRY_BUFFER`] before now? Handles both ms-encoded
/// (Node `Date.now()`; what Claude Code writes) and seconds-encoded (older /
/// hand-rolled files) timestamps via [`SECONDS_BOUNDARY_MS`].
pub(crate) fn is_expired(expires_at: u64) -> bool {
    let expires_at_ms = normalize_expiry_ms(expires_at);
    let buffer_ms = PRE_EXPIRY_BUFFER.as_millis() as u64;
    expires_at_ms.saturating_sub(buffer_ms) <= unix_now_ms()
}

pub(crate) fn normalize_expiry_ms(expires_at: u64) -> u64 {
    if expires_at < SECONDS_BOUNDARY_MS {
        expires_at.saturating_mul(1000)
    } else {
        expires_at
    }
}

pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether the POST could have consumed the refresh token — the at-most-once hinge.
/// Only a *provably non-consuming* failure is [`Definite`](RotateError::Definite)
/// (safe to retry next run); anything that might have reached and been processed by
/// the server is [`Ambiguous`](RotateError::Ambiguous) (fail closed, never re-send).
enum RotateError {
    Definite(String),
    Ambiguous(String),
}

/// RFC 6749 §5.2 token-endpoint error codes. Each means the authorization server
/// REJECTED the grant *without* issuing or rotating a token — so the refresh token
/// was provably not consumed and re-attempting later is safe. Any other failure (a
/// 429, a middlebox 4xx, a 5xx, an unparseable body) might follow consumption and is
/// treated as ambiguous.
const OAUTH_GRANT_REJECTION_CODES: &[&str] = &[
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
];

/// POST the provider's refresh-token grant, classifying every failure for the
/// at-most-once invariant.
///
/// Redirects are DISABLED: reqwest replays the body on 307/308, which would POST the
/// single-use grant twice in one call and trip reuse detection. A 3xx therefore
/// surfaces as a non-2xx and resolves to `Ambiguous`. `accept-encoding: identity`
/// because providers may gzip OAuth responses and the blocking client doesn't
/// auto-decode them.
fn post_refresh(
    codec: &dyn OAuthCodec,
    refresh_token: &str,
) -> std::result::Result<Value, RotateError> {
    let request = codec.refresh_request(refresh_token);
    let body = serde_json::to_vec(&request.body)
        // Serializing our own body can't fail in practice; if it did, nothing was sent.
        .map_err(|e| RotateError::Definite(format!("serialize refresh body: {e}")))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(REFRESH_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // Building the client never reaches the network → token intact.
        .map_err(|e| RotateError::Definite(format!("build refresh client: {e}")))?;

    let resp = match client
        .post(request.endpoint)
        .header("content-type", "application/json")
        .header("accept-encoding", "identity")
        .body(body)
        .send()
    {
        Ok(r) => r,
        // A pure connect failure provably never delivered the token. Exclude a
        // connect *timeout* (`is_connect() && is_timeout()`): a timeout can never be
        // proven pre-send, so it stays Ambiguous. Anything `is_connect` doesn't cover
        // falls to the `Err(_)` arm below → Ambiguous → fail closed; so the only way
        // a future reqwest could perturb this is by *broadening* `is_connect`, which
        // can only make us over-conservative (extra re-auth), never risk a reuse.
        Err(e) if e.is_connect() && !e.is_timeout() => {
            return Err(RotateError::Definite("refresh connect failed".to_string()));
        }
        // Timeout or any post-connect failure: the request may already be on the wire
        // → never re-send. The error Display is dropped (it can carry the request URL
        // — no token, but keep the reason minimal).
        Err(_) => {
            return Err(RotateError::Ambiguous(
                "refresh request failed mid-flight".to_string(),
            ));
        }
    };

    let status = resp.status();
    let read = resp.bytes();
    if status.is_success() {
        let bytes = read
            .map_err(|_| RotateError::Ambiguous("refresh succeeded but body unreadable".into()))?;
        return serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| RotateError::Ambiguous("refresh succeeded but body not JSON".into()));
    }
    Err(classify_non_2xx(status, &read.unwrap_or_default()))
}

/// Classify a non-2xx refresh response. Pure (no I/O) so the security-critical
/// decision is unit-testable without a network.
///
/// `Definite` ONLY for an RFC 6749 grant rejection — a recognized error code AND the
/// status the spec returns it with (400 for grant errors, 401 for `invalid_client`).
/// The status gate is load-bearing: a 3xx / 429 / 5xx carrying an OAuth-looking body
/// might follow consumption, so it stays `Ambiguous` regardless of body. The surfaced
/// reason carries only a sanitized known-shape error code — never the raw body or a
/// reflected value.
fn classify_non_2xx(status: reqwest::StatusCode, body: &[u8]) -> RotateError {
    let code = oauth_error_code(body);
    let clean_rejection = matches!(status.as_u16(), 400 | 401)
        && code
            .as_deref()
            .is_some_and(|c| OAUTH_GRANT_REJECTION_CODES.contains(&c));
    let summary = match code.as_deref().filter(|c| is_safe_error_code(c)) {
        Some(c) => format!("HTTP {status} ({c})"),
        None => format!("HTTP {status}"),
    };
    if clean_rejection {
        RotateError::Definite(format!("refresh rejected: {summary}"))
    } else {
        RotateError::Ambiguous(format!("refresh failed: {summary}"))
    }
}

/// The short OAuth `error` code from a token-endpoint error body, if present.
/// Returns ONLY the server-defined identifier — never the raw body or the free-text
/// `error_description` — so a surfaced reason can't echo back a secret an intermediary
/// might have reflected into its response.
fn oauth_error_code(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    if let Some(error) = value.get("error") {
        if let Some(code) = error.as_str() {
            return Some(code.to_owned());
        }
        if let Some(code) = error.get("code").and_then(Value::as_str) {
            return Some(code.to_owned());
        }
    }
    value.get("code").and_then(Value::as_str).map(str::to_owned)
}

/// Whether an OAuth `error` code is safe to surface in a log/error reason: the
/// snake_case shape RFC 6749 uses, length-capped. Guards against a middlebox or
/// hostile upstream reflecting a token / long attacker-controlled value into the
/// `error` field and having it echoed to stderr.
fn is_safe_error_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 40
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn codex_jwt(expiry_ms: u64) -> String {
        let payload = serde_json::to_vec(&serde_json::json!({
            "exp": expiry_ms / 1000,
        }))
        .unwrap();
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("e30.{payload}.pillbox-test-signature")
    }

    #[test]
    fn is_expired_normalizes_seconds_and_milliseconds() {
        let now_ms = unix_now_ms();
        let well_past_ms = now_ms - 24 * 60 * 60 * 1000;
        assert!(is_expired(well_past_ms));
        assert!(is_expired(well_past_ms / 1000)); // seconds-encoded

        let well_future_ms = now_ms + 24 * 60 * 60 * 1000;
        assert!(!is_expired(well_future_ms));
        assert!(!is_expired(well_future_ms / 1000));
    }

    #[test]
    fn is_expired_respects_pre_expiry_buffer() {
        // 30s in the future reports expired — we leave PRE_EXPIRY_BUFFER (5min) headroom.
        let near_future_ms = unix_now_ms() + 30 * 1000;
        assert!(is_expired(near_future_ms));
    }

    #[test]
    fn claude_decider_reads_tokens_and_staleness() {
        let provider = providers::provider_for("claude").unwrap();
        let d = provider.oauth_codec().unwrap();
        let fresh = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "AT",
                "refreshToken": "RT",
                "expiresAt": unix_now_ms() + 24 * 60 * 60 * 1000,
            }
        });
        assert_eq!(d.access_token(&fresh).as_deref(), Some("AT"));
        assert_eq!(d.refresh_token(&fresh).as_deref(), Some("RT"));
        assert!(!d.needs_refresh(&fresh));
        assert!(d.access_usable(&fresh));

        let near_true_expiry = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "AT",
                "refreshToken": "RT",
                "expiresAt": unix_now_ms() + 30 * 1000,
            }
        });
        assert!(d.needs_refresh(&near_true_expiry));
        assert!(d.access_usable(&near_true_expiry));

        let stale = serde_json::json!({
            "claudeAiOauth": { "accessToken": "AT", "refreshToken": "RT", "expiresAt": 1_700_000_000_000_u64 }
        });
        assert!(d.needs_refresh(&stale));
        assert!(!d.access_usable(&stale));

        // No expiresAt → no signal → don't force a POST.
        let no_expiry =
            serde_json::json!({ "claudeAiOauth": { "accessToken": "AT", "refreshToken": "RT" } });
        assert!(!d.needs_refresh(&no_expiry));
        assert!(!d.access_usable(&no_expiry));
    }

    #[test]
    fn codex_decider_uses_access_jwt_expiry() {
        let provider = providers::provider_for("codex").unwrap();
        let d = provider.oauth_codec().unwrap();
        let fresh_expiry = unix_now_ms() + 24 * 60 * 60 * 1000;
        let fresh = serde_json::json!({
            "tokens": {
                "access_token": codex_jwt(fresh_expiry),
                "refresh_token": "RT",
            }
        });
        assert_eq!(d.refresh_token(&fresh).as_deref(), Some("RT"));
        assert!(!d.needs_refresh(&fresh));
        assert!(d.access_usable(&fresh));
        assert_eq!(d.expiry_ms(&fresh), Some((fresh_expiry / 1000) * 1000));

        let near_expiry = serde_json::json!({
            "tokens": {
                "access_token": codex_jwt(unix_now_ms() + 30 * 1000),
                "refresh_token": "RT",
            }
        });
        assert!(d.needs_refresh(&near_expiry));
        assert!(d.access_usable(&near_expiry));

        let expired = serde_json::json!({
            "tokens": {
                "access_token": codex_jwt(1_700_000_000_000),
                "refresh_token": "RT",
            }
        });
        assert!(d.needs_refresh(&expired));
        assert!(!d.access_usable(&expired));

        let malformed = serde_json::json!({
            "tokens": { "access_token": "not-a-jwt", "refresh_token": "RT" }
        });
        assert!(d.needs_refresh(&malformed));
        assert!(!d.access_usable(&malformed));
    }

    #[test]
    fn codex_refresh_request_matches_0_151_json_contract() {
        let provider = providers::provider_for("codex").unwrap();
        let request = provider
            .oauth_codec()
            .unwrap()
            .refresh_request("STUB_REFRESH");
        assert_eq!(request.endpoint, "https://auth.openai.com/oauth/token");
        assert_eq!(
            request.body,
            serde_json::json!({
                "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
                "grant_type": "refresh_token",
                "refresh_token": "STUB_REFRESH",
            })
        );
    }

    #[test]
    fn classify_grant_rejection_is_definite_only_at_spec_status() {
        // invalid_grant at 400 → the AS rejected without issuing → provably intact.
        assert!(matches!(
            classify_non_2xx(
                reqwest::StatusCode::BAD_REQUEST,
                br#"{"error":"invalid_grant"}"#
            ),
            RotateError::Definite(_)
        ));
        // invalid_client at 401 → spec status for that code → Definite.
        assert!(matches!(
            classify_non_2xx(
                reqwest::StatusCode::UNAUTHORIZED,
                br#"{"error":"invalid_client"}"#
            ),
            RotateError::Definite(_)
        ));
        // Same code but a 5xx → might follow consumption → Ambiguous despite the body.
        assert!(matches!(
            classify_non_2xx(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                br#"{"error":"invalid_grant"}"#
            ),
            RotateError::Ambiguous(_)
        ));
        // 429 with no recognizable OAuth body → Ambiguous.
        assert!(matches!(
            classify_non_2xx(reqwest::StatusCode::TOO_MANY_REQUESTS, b"slow down"),
            RotateError::Ambiguous(_)
        ));

        // OpenAI wraps the OAuth code in an error object; recognize only the code,
        // never its free-text message.
        assert!(matches!(
            classify_non_2xx(
                reqwest::StatusCode::UNAUTHORIZED,
                br#"{"error":{"code":"invalid_grant","message":"do not surface me"}}"#
            ),
            RotateError::Definite(_)
        ));
    }

    #[test]
    fn error_reason_never_reflects_unsafe_bodies() {
        // A hostile/middlebox `error` value (uppercase, spaces, over-long) is dropped
        // from the surfaced summary so it can't echo a reflected secret to stderr.
        let RotateError::Ambiguous(reason) = classify_non_2xx(
            reqwest::StatusCode::BAD_GATEWAY,
            br#"{"error":"sk-ant-LEAKED SECRET VALUE"}"#,
        ) else {
            panic!("expected Ambiguous");
        };
        assert!(
            !reason.contains("LEAKED"),
            "reason leaked the body: {reason}"
        );
        assert!(
            !reason.contains("sk-ant"),
            "reason leaked the body: {reason}"
        );
        assert!(!is_safe_error_code("sk-ant-LEAKED SECRET VALUE"));
        assert!(is_safe_error_code("invalid_grant"));
    }

    #[test]
    fn apply_refresh_response_overwrites_oauth_block() {
        let provider = providers::provider_for("claude").unwrap();
        let codec = provider.oauth_codec().unwrap();
        let mut real = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "OLD_ACCESS",
                "refreshToken": "OLD_REFRESH",
                "expiresAt": 1_700_000_000_000_u64,
                "subscriptionType": "pro",
            }
        });
        let resp = serde_json::json!({
            "access_token": "NEW_ACCESS",
            "refresh_token": "NEW_REFRESH",
            "expires_in": 3600,
        });
        codec
            .apply_refresh_response(&mut real, &resp, "OLD_REFRESH", 2_000_000_000_000_u64)
            .expect("apply");

        let oauth = real.get("claudeAiOauth").unwrap();
        assert_eq!(
            oauth.get("accessToken").and_then(|v| v.as_str()),
            Some("NEW_ACCESS")
        );
        assert_eq!(
            oauth.get("refreshToken").and_then(|v| v.as_str()),
            Some("NEW_REFRESH")
        );
        assert_eq!(
            oauth.get("expiresAt").and_then(|v| v.as_u64()),
            Some(2_000_000_000_000_u64 + 3600 * 1000),
        );
        // Untouched fields preserved.
        assert_eq!(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            Some("pro")
        );
    }

    #[test]
    fn apply_refresh_response_preserves_old_refresh_when_response_omits_it() {
        let provider = providers::provider_for("claude").unwrap();
        let codec = provider.oauth_codec().unwrap();
        let mut real = serde_json::json!({
            "claudeAiOauth": { "accessToken": "OLD", "refreshToken": "PRESERVE_ME", "expiresAt": 0_u64 }
        });
        let resp = serde_json::json!({ "access_token": "NEW_ACCESS", "expires_in": 3600 });
        codec
            .apply_refresh_response(&mut real, &resp, "PRESERVE_ME", 0)
            .expect("apply");
        assert_eq!(
            real.pointer("/claudeAiOauth/refreshToken")
                .and_then(|v| v.as_str()),
            Some("PRESERVE_ME"),
        );
    }

    #[test]
    fn codex_refresh_response_updates_tokens_and_preserves_auth_fields() {
        let provider = providers::provider_for("codex").unwrap();
        let codec = provider.oauth_codec().unwrap();
        let expiry_ms = 2_000_000_000_000_u64;
        let mut real = serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "id_token": "OLD_ID",
                "access_token": codex_jwt(1_700_000_000_000),
                "refresh_token": "OLD_REFRESH",
                "account_id": "acct-preserve",
            },
            "last_refresh": "2026-01-01T00:00:00Z",
            "future_field": "preserve",
        });
        let new_access = codex_jwt(expiry_ms);
        let resp = serde_json::json!({
            "id_token": "NEW_ID",
            "access_token": new_access,
            "refresh_token": "NEW_REFRESH",
        });
        codec
            .apply_refresh_response(&mut real, &resp, "OLD_REFRESH", 1_999_000_000_000)
            .expect("apply");

        assert_eq!(
            real.pointer("/tokens/access_token").and_then(Value::as_str),
            resp.get("access_token").and_then(Value::as_str)
        );
        assert_eq!(
            real.pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("NEW_REFRESH")
        );
        assert_eq!(
            real.pointer("/tokens/id_token").and_then(Value::as_str),
            Some("NEW_ID")
        );
        assert_eq!(
            real.pointer("/tokens/account_id").and_then(Value::as_str),
            Some("acct-preserve")
        );
        assert_eq!(
            real.get("future_field").and_then(Value::as_str),
            Some("preserve")
        );
        assert_ne!(
            real.get("last_refresh").and_then(Value::as_str),
            Some("2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn codex_refresh_response_preserves_omitted_optional_tokens() {
        let provider = providers::provider_for("codex").unwrap();
        let codec = provider.oauth_codec().unwrap();
        let mut real = serde_json::json!({
            "tokens": {
                "id_token": "PRESERVE_ID",
                "access_token": codex_jwt(1_700_000_000_000),
                "refresh_token": "PRESERVE_REFRESH",
            }
        });
        let resp = serde_json::json!({
            "access_token": codex_jwt(2_000_000_000_000),
        });
        codec
            .apply_refresh_response(&mut real, &resp, "PRESERVE_REFRESH", 0)
            .expect("apply");
        assert_eq!(
            real.pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("PRESERVE_REFRESH")
        );
        assert_eq!(
            real.pointer("/tokens/id_token").and_then(Value::as_str),
            Some("PRESERVE_ID")
        );
    }

    #[test]
    fn pre_refresh_coalesces_fresh_codex_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let creds = serde_json::json!({
            "tokens": {
                "access_token": codex_jwt(unix_now_ms() + 24 * 60 * 60 * 1000),
                "refresh_token": "RT",
            }
        });
        std::fs::write(&path, serde_json::to_vec(&creds).unwrap()).unwrap();
        let refreshed = pre_refresh(&path, "codex")
            .expect("fresh creds coalesce")
            .expect("codex has broker");
        assert_eq!(refreshed, creds);
    }

    #[test]
    fn pre_refresh_skips_agents_without_a_broker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, b"{}").unwrap();
        assert!(matches!(pre_refresh(&path, "opencode"), Ok(None)));
    }
}
