//! Host-side drivers for the in-guest `codex app-server` bridge — the codex
//! analog of `sandbox::opencode`'s host helpers, split out of the guest bridge
//! ([`crate::sandbox::appserver`]) so the two halves (guest runtime vs host
//! client) live apart. Each call here is one HTTP request through the
//! [`SandboxHttp`] transport (a vsock forward, for the libkrun backend) to the
//! bridge's one-shot routes.
//!
//! Most of this is consumed only by the libkrun run path (codex-serve is
//! libkrun-only), hence the `allow(dead_code)` on the non-libkrun build;
//! [`send_turn`] is the exception — `session send` drives it on every build.

use serde_json::{json, Value};

use anyhow::Result;

use crate::errors::PillboxError;
use crate::sandbox::http::SandboxHttp;

/// TCP port the in-guest `appserver-host` HTTP API binds (loopback; reached by
/// the backend's [`SandboxHttp`] over the vsock forward). Distinct constant from
/// opencode's so the two server agents can't be confused, even though the vsock
/// forward maps each to the same guest-side port today.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const BRIDGE_PORT: u16 = 4097;

/// Filename (under the agent home) the bridge appends codex notifications to —
/// the §0 NDJSON capture, the codex analog of opencode's `EVENTS_FILE`. Lives in
/// the shared/CoW home so it persists + is host-readable for the drain.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const EVENTS_FILE: &str = ".pillbox-codex-appserver-events.ndjson";

#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
const ACTION: &str = "run (codex-serve)";

/// Poll `GET /health` until the bridge answers `200` (codex boot + the
/// handshake take a moment), bounded so a dead bridge fails loud not hangs.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn wait_ready(http: &dyn SandboxHttp) -> Result<()> {
    for _ in 0..60 {
        if let Ok(resp) = http.request("GET", "/health", None) {
            if resp.status == 200 {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(PillboxError::runtime(ACTION, "codex app-server bridge didn't become ready in 30s").into())
}

/// `POST /session` → the codex `threadId` the bridge started at boot (the
/// agent-native session id `turn/start` targets; stored on the session record).
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn create_session(http: &dyn SandboxHttp) -> Result<String> {
    let resp = http.request("POST", "/session", Some("{}"))?;
    let body = resp.body.trim();
    let value: Value = serde_json::from_str(body).map_err(|_| {
        PillboxError::runtime(
            ACTION,
            format!("create session: unexpected response: {body}"),
        )
    })?;
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            PillboxError::runtime(ACTION, format!("create session: no thread id in {body}")).into()
        })
}

/// Drive the session: `POST /turn` with the prompt text. The bridge issues
/// `turn/start` and the turn streams as notifications to the events file (read
/// via the §0 drain). Any 2xx is success (the bridge returns 204).
pub(crate) fn send_turn(http: &dyn SandboxHttp, text: &str) -> Result<()> {
    let body = json!({ "text": text }).to_string();
    let resp = http.request("POST", "/turn", Some(&body))?;
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(PillboxError::runtime(
            "session send",
            format!("codex app-server turn failed (HTTP {})", resp.status),
        )
        .into())
    }
}
