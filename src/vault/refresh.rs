//! Host-side OAuth token refresh — the Claude [`RefreshAdapter`].
//!
//! Plugs Claude Code's wire shape into the provider-agnostic
//! [`super::token_store::TokenStore`]: which field is the refresh token,
//! when a refresh is due, and how to POST it. The store owns the locking,
//! the at-most-once `pending` discipline, and the atomic write; this file
//! owns only the Anthropic-specific bits.
//!
//! The store drives this at vault session start (`provision_oauth_mount`),
//! **before** the proxy is up, so the stub-credentials file mounted into
//! the sandbox carries a freshly-rotated token with a future `expiresAt`.
//! Mirrors what Claude Code does on a developer's laptop: the agent never
//! sees a 401 on its first call because the wrapper refreshes proactively
//! whenever the stored access token has passed its expiry. Without it the
//! sandbox would inherit whatever the last `pillbox auth login` wrote —
//! normally hours-to-days stale by the next session.
//!
//! Currently Claude-only. Codex / OpenAI / GitHub each have their own
//! refresh shapes; each gets its own adapter when it becomes similarly
//! stale-prone (see `refresh_adapter_for`).

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::errors::PillboxError;

use super::token_store::{EnsureOutcome, RefreshAdapter, RotateError, TokenStore};

/// Anthropic OAuth `client_id` for the Claude Code CLI flow. The same
/// value Claude Code itself sends when *it* refreshes. Hardcoded
/// because there's no public API to discover it; pillbox just needs
/// the canonical one Anthropic expects.
const CLAUDE_OAUTH_CLIENT_ID: &str = "claude_code";

/// Anthropic OAuth `/oauth/token` endpoint Claude Code's current
/// release talks to. The vault provider intercepts both this and the
/// legacy `console.anthropic.com` host; the pre-refresh path goes
/// straight to the canonical host so it can run *before* the proxy
/// is even up.
const CLAUDE_OAUTH_ENDPOINT: &str = "https://platform.claude.com/oauth/token";

/// Upper bound (in ms) below which a unix timestamp is *certainly*
/// in seconds. 1e11 ms is the year 5138 — no real seconds-encoded
/// timestamp will ever exceed this, so anything smaller is safely
/// scaled to ms.
const SECONDS_BOUNDARY_MS: u64 = 100_000_000_000;

/// Refresh tokens this far before their actual `expiresAt` so we
/// don't race a near-expiry token against the request's round-trip
/// latency.
const PRE_EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;

/// HTTP timeout for the upstream refresh call. Anthropic's OAuth
/// endpoint typically responds in <1s; 30s buys headroom for a
/// degraded edge without stalling pillbox startup forever.
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Default access-token lifetime to assume when the upstream
/// response omits `expires_in`. Matches Anthropic's documented
/// default at time of writing.
const FALLBACK_EXPIRES_IN_SECS: u64 = 3600;

/// The Claude Code OAuth shape, plugged into the [`TokenStore`].
///
/// [`TokenStore`]: super::token_store::TokenStore
pub(crate) struct ClaudeRefreshAdapter;

impl RefreshAdapter for ClaudeRefreshAdapter {
    fn refresh_token(&self, creds: &serde_json::Value) -> Option<String> {
        creds
            .pointer("/claudeAiOauth/refreshToken")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    fn needs_refresh(&self, creds: &serde_json::Value) -> bool {
        // No `expiresAt` to read → no staleness signal → don't force a POST
        // (matches the pre-TokenStore behavior, which simply skipped refresh
        // when the field was absent).
        creds
            .pointer("/claudeAiOauth/expiresAt")
            .and_then(|v| v.as_u64())
            .map(is_expired)
            .unwrap_or(false)
    }

    fn rotate(&self, creds: &mut serde_json::Value) -> std::result::Result<(), RotateError> {
        let refresh_token = self.refresh_token(creds).ok_or_else(|| {
            RotateError::Definite("no refreshToken in claudeAiOauth block".to_string())
        })?;
        let resp = post_refresh(&refresh_token)?;
        // A 2xx came back, so the upstream already consumed the old refresh
        // token — if we can't splice the new pair in, the token is gone but
        // we lack its replacement: Ambiguous (leave `pending` set, re-auth,
        // never re-send the consumed token).
        apply_refresh_response(creds, &resp, &refresh_token, unix_now_ms()).map_err(|e| {
            RotateError::Ambiguous(format!("upstream rotated but response unusable: {e}"))
        })?;
        Ok(())
    }
}

/// The [`RefreshAdapter`] for `agent_id`, or `None` when pillbox has no
/// proactive-refresh shape for that agent yet (its pre-refresh is then
/// skipped and the agent's own retry-on-401 handles staleness). Only
/// `claude` is wired today.
pub(super) fn refresh_adapter_for(agent_id: &str) -> Option<Box<dyn RefreshAdapter>> {
    match agent_id {
        "claude" => Some(Box::new(ClaudeRefreshAdapter)),
        _ => None,
    }
}

/// How long a start-of-run pre-refresh waits for the rotation lock before giving
/// up (→ `LockBusy` → fail closed). Sized above the refresh HTTP timeout (30s)
/// so a normal convoy *coalesces*: the lock-holder rotates once and each waiter,
/// on acquiring, re-reads the fresh creds and returns `Fresh` — none spuriously
/// hits the fail-closed `LockBusy` path. If a holder stays wedged past this
/// bound, failing closed is correct: it's mid-POST of a stale token, so this
/// waiter's token is stale too. Giving up never risks a double-POST — a waiter
/// that times out does not POST anything.
const PRE_REFRESH_LOCK_WAIT: Duration = Duration::from_secs(35);

/// The coordinated start-of-run pre-refresh both backends call (docker's
/// `provision_oauth_mount`, libkrun's `prepare_launch`). Routes the creds at
/// `creds_path` through the single-writer [`TokenStore`] so concurrent launches
/// (`dispatch -k`, overlapping `--detach`) rotate the shared refresh token at
/// most once.
///
/// - `Ok(Some(creds))` — fresh (already, or rotated this call); the on-disk file
///   is updated atomically under the lock and the live creds returned.
/// - `Ok(None)` — `agent_id` has no refresh adapter (pre-refresh skipped).
/// - `Err` — **fails closed.** The only safe credential to lease is a freshly
///   rotated one; a stale, refresh-capable credential would let the guest
///   re-POST a maybe-consumed / being-rotated token through the in-proxy path
///   (uncoordinated until slice 2b) and trip reuse-revoke. See the design doc.
pub(crate) fn pre_refresh(creds_path: &Path, agent_id: &str) -> Result<Option<serde_json::Value>> {
    let Some(adapter) = refresh_adapter_for(agent_id) else {
        return Ok(None);
    };
    let store = TokenStore::new(creds_path.to_path_buf(), PRE_REFRESH_LOCK_WAIT);
    pre_refresh_outcome(store.ensure_fresh(adapter.as_ref())?, agent_id)
}

/// Map an [`EnsureOutcome`] to the pre-refresh caller contract — the fail-closed
/// policy, in one place so both backends share it (and it's unit-testable
/// without a network). `Fresh` → adopt; everything else → abort with a clear
/// next-step.
fn pre_refresh_outcome(
    outcome: EnsureOutcome,
    agent_id: &str,
) -> Result<Option<serde_json::Value>> {
    match outcome {
        EnsureOutcome::Fresh(creds) => Ok(Some(creds)),
        EnsureOutcome::ReauthRequired(reason) => Err(PillboxError::runtime(
            "vault",
            format!("could not establish a fresh OAuth token for `{agent_id}`: {reason}"),
        )
        // The cause may be transient (a connect blip, token intact) or terminal
        // (a revoked family) — the reason text distinguishes, but the remedy that
        // covers both is "re-run, then re-auth if it persists". A bare "login"
        // would mislead on the common network-blip case.
        .with_next(format!(
            "retry the command; if it persists, run `pillbox auth login --agent {agent_id}`"
        ))
        .into()),
        EnsureOutcome::LockBusy => Err(PillboxError::runtime(
            "vault",
            format!(
                "another session is refreshing `{agent_id}` credentials and did not release the \
                 rotation lock within {PRE_REFRESH_LOCK_WAIT:?}"
            ),
        )
        .with_next("retry in a moment")
        .into()),
    }
}

/// Has `expires_at` passed [`PRE_EXPIRY_BUFFER_MS`] before now? Handles
/// both ms-encoded (Node `Date.now()` convention; what Claude Code
/// writes) and seconds-encoded (older / hand-rolled credential
/// files) timestamps via [`SECONDS_BOUNDARY_MS`].
fn is_expired(expires_at: u64) -> bool {
    let expires_at_ms = if expires_at < SECONDS_BOUNDARY_MS {
        expires_at.saturating_mul(1000)
    } else {
        expires_at
    };
    expires_at_ms.saturating_sub(PRE_EXPIRY_BUFFER_MS) <= unix_now_ms()
}

fn unix_now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// RFC 6749 §5.2 token-endpoint error codes. Each means the authorization
/// server REJECTED the grant *without* issuing or rotating a token — so the
/// refresh token was provably not consumed and re-attempting later is safe.
/// Any other failure (a 429 rate-limit, a middlebox 4xx, a 5xx, an
/// unparseable body) might follow consumption and is treated as ambiguous.
const OAUTH_GRANT_REJECTION_CODES: &[&str] = &[
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
];

/// The short OAuth `error` code from a token-endpoint error body, if present.
/// Returns ONLY the server-defined identifier — never the raw body or the
/// free-text `error_description` — so a surfaced reason can't echo back a
/// secret an intermediary might have reflected into its response.
fn oauth_error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
}

/// POST the refresh-token grant to [`CLAUDE_OAUTH_ENDPOINT`] and return
/// the decoded JSON body, classifying every failure as [`RotateError`].
///
/// The classification is the at-most-once hinge: it answers "could this POST
/// have consumed the token?" Only a *provably non-consuming* failure may be
/// [`RotateError::Definite`] (which clears the `pending` marker); anything that
/// might have reached and been processed by the server is
/// [`RotateError::Ambiguous`] (leave `pending` set, re-auth, never re-send):
///  - A pre-send failure — a connect error (DNS/TCP/TLS handshake/refused, but
///    not a connect *timeout*) or a body/client build error — never put the
///    token on the wire → `Definite`.
///  - A non-2xx whose body is a recognized OAuth grant-rejection
///    ([`OAUTH_GRANT_REJECTION_CODES`], e.g. `invalid_grant`) → the AS rejected
///    the grant without rotating → `Definite`.
///  - A timeout, a 5xx, a 429, a middlebox 4xx, or an unparseable/2xx-but-bad
///    body → the grant MAY have been processed → `Ambiguous`.
///
/// Redirects are DISABLED: a single-use grant must never be auto-resent to a
/// redirect target (reqwest replays the body on 307/308), which would POST the
/// same refresh token twice in one call and trip reuse detection. A 3xx
/// therefore surfaces as a non-2xx and resolves to `Ambiguous`.
///
/// `accept-encoding: identity` because Anthropic gzips OAuth responses by
/// default and the blocking-reqwest client doesn't auto-decode them.
fn post_refresh(refresh_token: &str) -> std::result::Result<serde_json::Value, RotateError> {
    let body = serde_json::to_vec(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_OAUTH_CLIENT_ID,
    }))
    // Serializing our own body can't fail in practice; if it does, nothing
    // was sent → Definite (token intact).
    .map_err(|e| RotateError::Definite(format!("serialize refresh body: {e}")))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(REFRESH_HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // Building the client never reaches the network → token intact.
        .map_err(|e| RotateError::Definite(format!("build refresh client: {e}")))?;

    let resp = match client
        .post(CLAUDE_OAUTH_ENDPOINT)
        .header("content-type", "application/json")
        .header("accept-encoding", "identity")
        .body(body)
        .send()
    {
        Ok(r) => r,
        // A pure connect failure provably never delivered the token. Exclude a
        // connect *timeout* (`is_connect() && is_timeout()`): a timeout can
        // never be proven pre-send, so it must stay Ambiguous even if a future
        // `connect_timeout` makes reqwest classify it as a connect error.
        Err(e) if e.is_connect() && !e.is_timeout() => {
            return Err(RotateError::Definite("refresh connect failed".to_string()));
        }
        // Timeout or any post-connect failure: the request may already be on
        // the wire → never re-send. The error Display is intentionally dropped
        // (it can carry the request URL — no token, but keep the reason
        // minimal).
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
        return serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|_| RotateError::Ambiguous("refresh succeeded but body not JSON".into()));
    }

    let body = read.unwrap_or_default();
    Err(classify_non_2xx(status, &body))
}

/// Classify a non-2xx refresh response for the at-most-once invariant. Pure (no
/// I/O) so the security-critical decision is unit-testable without a network.
///
/// Definite ONLY for an RFC 6749 grant rejection — a recognized error code AND
/// the status the spec returns it with (400 for grant errors, 401 for
/// `invalid_client`). The status gate is load-bearing: a 3xx / 429 / 5xx that
/// happens to carry an OAuth-looking body might follow consumption, so it stays
/// Ambiguous regardless of the body. The surfaced reason carries only a
/// sanitized known-shape error code — never the raw body or a reflected value.
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

/// Splice new `access_token` / `refresh_token` / `expiresAt` into the
/// stored `claudeAiOauth` block. `refresh_token` may be omitted from the
/// response (Anthropic doesn't always rotate it); in that case the old
/// refresh token is preserved so the next session can still talk upstream.
fn apply_refresh_response(
    real: &mut serde_json::Value,
    resp: &serde_json::Value,
    old_refresh: &str,
    now_ms: u64,
) -> Result<()> {
    let new_access = resp
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PillboxError::runtime("vault", "refresh response missing access_token"))?
        .to_string();
    let new_refresh = resp
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or(old_refresh)
        .to_string();
    let expires_in_secs = resp
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(FALLBACK_EXPIRES_IN_SECS);
    let new_expires_at_ms = now_ms.saturating_add(expires_in_secs.saturating_mul(1000));

    let oauth = real
        .get_mut("claudeAiOauth")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| PillboxError::runtime("vault", "claudeAiOauth block disappeared"))?;
    oauth.insert(
        "accessToken".to_string(),
        serde_json::Value::String(new_access),
    );
    oauth.insert(
        "refreshToken".to_string(),
        serde_json::Value::String(new_refresh),
    );
    oauth.insert(
        "expiresAt".to_string(),
        serde_json::Value::Number(serde_json::Number::from(new_expires_at_ms)),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // A timestamp 30 seconds in the future reports expired because we
        // leave PRE_EXPIRY_BUFFER (5min) of headroom.
        let near_future_ms = unix_now_ms() + 30 * 1000;
        assert!(is_expired(near_future_ms));
    }

    #[test]
    fn adapter_reads_refresh_token_and_staleness() {
        let adapter = ClaudeRefreshAdapter;
        let fresh = serde_json::json!({
            "claudeAiOauth": {
                "refreshToken": "RT",
                "expiresAt": unix_now_ms() + 24 * 60 * 60 * 1000,
            }
        });
        assert_eq!(adapter.refresh_token(&fresh).as_deref(), Some("RT"));
        assert!(!adapter.needs_refresh(&fresh));

        let stale = serde_json::json!({
            "claudeAiOauth": { "refreshToken": "RT", "expiresAt": 1_700_000_000_000_u64 }
        });
        assert!(adapter.needs_refresh(&stale));

        // No expiresAt → no signal → don't force a POST.
        let no_exp = serde_json::json!({ "claudeAiOauth": { "refreshToken": "RT" } });
        assert!(!adapter.needs_refresh(&no_exp));

        // No refreshToken → None (rotate will resolve to ReauthRequired).
        let no_rt = serde_json::json!({ "claudeAiOauth": { "expiresAt": 0_u64 } });
        assert_eq!(adapter.refresh_token(&no_rt), None);
    }

    #[test]
    fn rotate_without_refresh_token_is_definite() {
        let adapter = ClaudeRefreshAdapter;
        let mut creds = serde_json::json!({ "claudeAiOauth": { "expiresAt": 0_u64 } });
        match adapter.rotate(&mut creds) {
            Err(RotateError::Definite(_)) => {}
            other => panic!("expected Definite, got {other:?}"),
        }
    }

    #[test]
    fn apply_refresh_response_overwrites_oauth_block() {
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
        let now_ms = 2_000_000_000_000_u64;
        apply_refresh_response(&mut real, &resp, "OLD_REFRESH", now_ms).expect("apply");

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
            Some(now_ms + 3600 * 1000),
        );
        assert_eq!(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            Some("pro"),
        );
    }

    #[test]
    fn apply_refresh_response_preserves_old_refresh_when_response_omits_it() {
        let mut real = serde_json::json!({
            "claudeAiOauth": { "accessToken": "OLD", "refreshToken": "PRESERVE_ME", "expiresAt": 0_u64 }
        });
        let resp = serde_json::json!({
            "access_token": "NEW_ACCESS",
            "expires_in": 3600,
        });
        apply_refresh_response(&mut real, &resp, "PRESERVE_ME", 0).expect("apply");
        assert_eq!(
            real.pointer("/claudeAiOauth/refreshToken")
                .and_then(|v| v.as_str()),
            Some("PRESERVE_ME"),
        );
    }

    #[test]
    fn refresh_adapter_for_known_agents() {
        assert!(refresh_adapter_for("claude").is_some());
        assert!(refresh_adapter_for("codex").is_none());
        assert!(refresh_adapter_for("nope").is_none());
    }

    #[test]
    fn oauth_error_code_extracts_only_the_code() {
        assert_eq!(
            oauth_error_code(br#"{"error":"invalid_grant","error_description":"revoked"}"#)
                .as_deref(),
            Some("invalid_grant"),
        );
        // No error field / not JSON / non-string error → None.
        assert_eq!(oauth_error_code(br#"{"detail":"nope"}"#), None);
        assert_eq!(oauth_error_code(b"<html>429</html>"), None);
        assert_eq!(oauth_error_code(b""), None);
    }

    #[test]
    fn only_grant_rejection_codes_are_clean_rejections() {
        // A clean rejection (token not consumed) → eligible for Definite.
        assert!(OAUTH_GRANT_REJECTION_CODES.contains(&"invalid_grant"));
        // A rate-limit is NOT a grant rejection — it may follow consumption →
        // must stay Ambiguous, so it must not be in the whitelist.
        assert!(!OAUTH_GRANT_REJECTION_CODES.contains(&"rate_limited"));
        assert!(!OAUTH_GRANT_REJECTION_CODES.contains(&"server_error"));
    }

    fn status(n: u16) -> reqwest::StatusCode {
        reqwest::StatusCode::from_u16(n).unwrap()
    }

    #[test]
    fn classify_definite_only_on_grant_rejection_status_and_code() {
        // The one Definite case: 400 + a recognized grant-rejection code.
        assert!(matches!(
            classify_non_2xx(status(400), br#"{"error":"invalid_grant"}"#),
            RotateError::Definite(_)
        ));
        // 401 invalid_client (client-auth failure) is also a clean rejection.
        assert!(matches!(
            classify_non_2xx(status(401), br#"{"error":"invalid_client"}"#),
            RotateError::Definite(_)
        ));
    }

    #[test]
    fn classify_status_gate_keeps_maybe_consumed_failures_ambiguous() {
        // The load-bearing fix: an OAuth-looking body on a non-grant-rejection
        // status MUST stay Ambiguous (the POST may have been processed).
        for s in [301, 429, 500, 502] {
            assert!(
                matches!(
                    classify_non_2xx(status(s), br#"{"error":"invalid_grant"}"#),
                    RotateError::Ambiguous(_)
                ),
                "HTTP {s} with a grant-rejection body must stay Ambiguous",
            );
        }
        // A 4xx that isn't a recognized grant rejection (e.g. 403, or 400 with a
        // rate-limit code) stays Ambiguous too.
        assert!(matches!(
            classify_non_2xx(status(403), br#"{"error":"forbidden"}"#),
            RotateError::Ambiguous(_)
        ));
        assert!(matches!(
            classify_non_2xx(status(400), br#"{"error":"rate_limited"}"#),
            RotateError::Ambiguous(_)
        ));
    }

    #[test]
    fn classify_never_surfaces_a_reflected_or_oversized_error_value() {
        // A hostile/middlebox body that reflects a token-shaped value into
        // `error` must NOT be echoed into the surfaced reason.
        let reflected = format!(r#"{{"error":"{}"}}"#, "sk-ant-ort01-REALLOOKINGTOKEN");
        let err = classify_non_2xx(status(400), reflected.as_bytes());
        let RotateError::Ambiguous(reason) = err else {
            panic!("dashes + uppercase fail the safe-code charset → not a grant rejection");
        };
        assert!(
            !reason.contains("sk-ant-ort01-REALLOOKINGTOKEN"),
            "got {reason}"
        );
        assert!(reason.contains("HTTP 400"));
    }

    #[test]
    fn is_safe_error_code_accepts_rfc_shapes_only() {
        assert!(is_safe_error_code("invalid_grant"));
        assert!(is_safe_error_code("invalid_client"));
        assert!(!is_safe_error_code("")); // empty
        assert!(!is_safe_error_code("Has-Dashes-And-Caps")); // reflected token shape
        assert!(!is_safe_error_code(&"x".repeat(41))); // oversized
        assert!(!is_safe_error_code("has spaces"));
    }

    #[test]
    fn pre_refresh_outcome_is_fail_closed_except_fresh() {
        // Fresh → adopt the rotated creds.
        let creds = serde_json::json!({ "claudeAiOauth": { "accessToken": "AT" } });
        let out = pre_refresh_outcome(EnsureOutcome::Fresh(creds.clone()), "claude").unwrap();
        assert_eq!(out, Some(creds));

        // ReauthRequired and LockBusy both abort (fail closed), with the
        // actionable next-step surfaced.
        let reauth = pre_refresh_outcome(EnsureOutcome::ReauthRequired("revoked".into()), "claude");
        let msg = format!("{:#}", reauth.unwrap_err());
        assert!(
            msg.contains("pillbox auth login --agent claude"),
            "got {msg}"
        );

        let busy = pre_refresh_outcome(EnsureOutcome::LockBusy, "claude");
        assert!(busy.is_err(), "LockBusy must fail closed");
    }
}
