//! Host-side OAuth token refresh.
//!
//! Runs at vault session start, **before** the proxy is up, so the
//! stub-credentials file we mount into the sandbox carries
//! freshly-rotated tokens with a future `expiresAt`. Mirrors what
//! Claude Code does on a developer's laptop: the agent never sees a
//! 401 on the first call because the wrapper refreshes proactively
//! whenever the stored access token has passed its expiry.
//!
//! Without this path the sandbox would inherit whatever
//! `~/.pillbox/global/auth/<agent>/.<creds>` had at last
//! `pillbox auth login`, which is normally hours-to-days stale by
//! the next session — the agent would 401 on its first request,
//! refresh through the proxy, and recover, but the user saw a
//! gratuitous error.
//!
//! Currently Claude-only (`agent_id == "claude"`). Codex / OpenAI /
//! GitHub each have their own refresh shapes; generalize when those
//! become similarly stale-prone. The function is a `pub(super)`
//! plain-fn rather than a trait so the dispatch stays in one
//! `match` and the wire shape is reviewable in one file.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::errors::PillboxError;

use super::session::write_private;

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
const PRE_EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

/// HTTP timeout for the upstream refresh call. Anthropic's OAuth
/// endpoint typically responds in <1s; 30s buys headroom for a
/// degraded edge without stalling pillbox startup forever.
const REFRESH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Default access-token lifetime to assume when the upstream
/// response omits `expires_in`. Matches Anthropic's documented
/// default at time of writing.
const FALLBACK_EXPIRES_IN: Duration = Duration::from_secs(3600);

/// Refresh the stored `claudeAiOauth` tokens in `real` if
/// `expiresAt` has passed (or is within [`PRE_EXPIRY_BUFFER`]), and
/// persist the new pair to `creds_path` so subsequent sessions also
/// start fresh.
///
/// No-op for non-Claude agents — Codex / OpenAI / GitHub each have
/// their own refresh shapes and we haven't generalized this yet.
///
/// All failures bubble up; the caller logs and falls back to the
/// stored tokens (the vault's stub-swap still works as long as the
/// access token is somehow valid OR the agent's own retry-on-401
/// triggers a fresh refresh through the proxy).
pub(super) fn refresh_real_if_expired(
    real: &mut serde_json::Value,
    agent_id: &str,
    creds_path: &Path,
) -> Result<()> {
    if agent_id != "claude" {
        return Ok(());
    }
    let Some(expires_at) = real
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(|v| v.as_u64())
    else {
        return Ok(());
    };
    if !is_expired(expires_at) {
        return Ok(());
    }

    let refresh_token = real
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PillboxError::runtime("vault", "no refreshToken in claudeAiOauth block"))?
        .to_string();

    let resp = post_refresh(&refresh_token)?;
    let now_ms = unix_now_ms();
    apply_refresh_response(real, &resp, &refresh_token, now_ms)?;

    let new_bytes = serde_json::to_string_pretty(real)
        .map_err(|e| PillboxError::runtime("vault", format!("serialize refreshed creds: {e}")))?;
    write_private(creds_path, &new_bytes)?;
    Ok(())
}

/// Has `expires_at` passed [`PRE_EXPIRY_BUFFER`] before now? Handles
/// both ms-encoded (Node `Date.now()` convention; what Claude Code
/// writes) and seconds-encoded (older / hand-rolled credential
/// files) timestamps via [`SECONDS_BOUNDARY_MS`].
fn is_expired(expires_at: u64) -> bool {
    let expires_at_ms = if expires_at < SECONDS_BOUNDARY_MS {
        expires_at.saturating_mul(1000)
    } else {
        expires_at
    };
    let buffer_ms = PRE_EXPIRY_BUFFER.as_millis() as u64;
    expires_at_ms.saturating_sub(buffer_ms) <= unix_now_ms()
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// POST the refresh-token grant to [`CLAUDE_OAUTH_ENDPOINT`] and
/// return the decoded JSON body. `accept-encoding: identity` because
/// Anthropic gzips OAuth responses by default and the
/// blocking-reqwest client doesn't auto-decode them (same root cause
/// as the gzip bug fixed for the proxied refresh path in
/// [`super::providers::anthropic`]).
fn post_refresh(refresh_token: &str) -> Result<serde_json::Value> {
    let body = serde_json::to_vec(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLAUDE_OAUTH_CLIENT_ID,
    }))
    .map_err(|e| PillboxError::runtime("vault", format!("serialize refresh body: {e}")))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(REFRESH_HTTP_TIMEOUT)
        .build()
        .map_err(|e| PillboxError::runtime("vault", format!("build refresh client: {e}")))?;
    let resp = client
        .post(CLAUDE_OAUTH_ENDPOINT)
        .header("content-type", "application/json")
        .header("accept-encoding", "identity")
        .body(body)
        .send()
        .map_err(|e| PillboxError::runtime("vault", format!("refresh request: {e}")))?;

    let status = resp.status();
    let resp_bytes = resp
        .bytes()
        .map_err(|e| PillboxError::runtime("vault", format!("refresh response read: {e}")))?;
    if !status.is_success() {
        return Err(PillboxError::runtime(
            "vault",
            format!(
                "refresh returned HTTP {status}: {}",
                String::from_utf8_lossy(&resp_bytes)
            ),
        )
        .with_next("pillbox auth login --agent claude   # re-authenticate".to_string())
        .into());
    }
    serde_json::from_slice::<serde_json::Value>(&resp_bytes).map_err(|e| {
        PillboxError::runtime("vault", format!("refresh response not JSON: {e}")).into()
    })
}

/// Splice new `access_token` / `refresh_token` / `expiresAt` into
/// the stored `claudeAiOauth` block. `refresh_token` may be omitted
/// from the response (Anthropic doesn't always rotate it); in that
/// case the old refresh token is preserved so the next session can
/// still talk to the upstream.
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
        .unwrap_or_else(|| FALLBACK_EXPIRES_IN.as_secs());
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
        // Past: any time before now-buffer should report expired.
        let now_ms = unix_now_ms();
        let well_past_ms = now_ms - 24 * 60 * 60 * 1000;
        assert!(is_expired(well_past_ms));
        assert!(is_expired(well_past_ms / 1000)); // seconds-encoded

        // Future: well-after-now should NOT be expired.
        let well_future_ms = now_ms + 24 * 60 * 60 * 1000;
        assert!(!is_expired(well_future_ms));
        assert!(!is_expired(well_future_ms / 1000));
    }

    #[test]
    fn is_expired_respects_pre_expiry_buffer() {
        // A timestamp 30 seconds in the future should report expired
        // because we leave PRE_EXPIRY_BUFFER (5min) of headroom.
        let near_future_ms = unix_now_ms() + 30 * 1000;
        assert!(is_expired(near_future_ms));
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
        // Untouched fields preserved.
        assert_eq!(
            oauth.get("subscriptionType").and_then(|v| v.as_str()),
            Some("pro"),
        );
    }

    #[test]
    fn apply_refresh_response_preserves_old_refresh_when_response_omits_it() {
        let mut real = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "OLD",
                "refreshToken": "PRESERVE_ME",
                "expiresAt": 0_u64,
            }
        });
        let resp = serde_json::json!({
            "access_token": "NEW_ACCESS",
            // no refresh_token field — Anthropic doesn't always rotate
            "expires_in": 3600,
        });
        apply_refresh_response(&mut real, &resp, "PRESERVE_ME", 0).expect("apply");
        assert_eq!(
            real.pointer("/claudeAiOauth/refreshToken")
                .and_then(|v| v.as_str()),
            Some("PRESERVE_ME"),
        );
    }
}
