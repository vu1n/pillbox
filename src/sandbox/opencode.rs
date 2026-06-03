//! Server-mode (opencode) sandbox bridge — talk to a headless `opencode serve`
//! running *inside* the sandbox over its HTTP API.
//!
//! opencode is an [`Integration::Server`](crate::agents::Integration) agent: it
//! runs as an HTTP server inside the sandbox and we drive/read it over its API
//! rather than a PTY. Every call here goes through a [`SandboxHttp`] transport,
//! so the bridge is backend-agnostic: docker supplies `docker exec curl`,
//! libkrun a real HTTP client over a forwarded vsock socket.
//!
//! - [`serve_args`] — the in-sandbox command (`opencode serve …`).
//! - [`wait_ready`] — poll `/doc` until the server answers.
//! - [`create_session`] — `POST /session` → the opencode session id.
//! - [`send_prompt`] — `POST /session/{id}/prompt_async` (the streaming drive).
//! - [`spawn_event_bridge`] — `GET /event` (SSE) → [`drain_sse`] → durable log.
//!
//! Routes/shapes are the bare ones (`/session`, not `/api/session`) verified
//! live against opencode 1.15.10; see docs/opencode-integration.md.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;

use crate::errors::PillboxError;
use crate::events::log::SessionLog;
use crate::events::opencode::drain_sse;
use crate::events::transcripts::TailerHandle;
use crate::sandbox::http::SandboxHttp;

const ACTION: &str = "run (opencode server)";

/// Port the in-sandbox `opencode serve` listens on (localhost-only; reached by
/// the backend's [`SandboxHttp`] transport, so no auth/publish needed).
pub(crate) const SERVE_PORT: u16 = 4096;

/// Default model when `--model` isn't given. `provider/modelID`. opencode's
/// `prompt_async` requires a model and the user's config sets no default, so we
/// supply one; override with `pillbox run --agent opencode --model …`.
pub(crate) const DEFAULT_MODEL: &str = "zai-coding-plan/glm-4.5-air";

/// Filename (under the agent home) the in-sandbox `/event` capture is appended
/// to — opencode's durable, gateway-free §0 transcript. A guest-side `curl -N
/// /event` loop writes raw SSE here; because it lives in the shared/CoW home it
/// persists + is host-readable, so the host drains it (replay + follow) on
/// `watch`/`subscribe` and captures completely even for a late reader — the same
/// file-transcript shape claude/codex use, no always-on host process. See
/// [`crate::events::opencode::FollowReader`]. (Consumed by the libkrun file
/// path; docker §0 still uses the live bridge.)
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const EVENTS_FILE: &str = ".pillbox-opencode-events.sse";

/// The in-sandbox command: a headless opencode server bound to localhost.
pub(crate) fn serve_args() -> Vec<String> {
    [
        "opencode",
        "serve",
        "--port",
        &SERVE_PORT.to_string(),
        "--hostname",
        "127.0.0.1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Poll `GET /doc` until the server answers `200` (the migration + boot can take
/// a few seconds), bounded so a dead server fails loud instead of hanging.
pub(crate) fn wait_ready(http: &dyn SandboxHttp) -> Result<()> {
    for _ in 0..60 {
        if let Ok(resp) = http.request("GET", "/doc", None) {
            if resp.status == 200 {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(PillboxError::runtime(ACTION, "opencode server didn't become ready in 30s").into())
}

/// `POST /session` → the new opencode session id (`ses_…`).
pub(crate) fn create_session(http: &dyn SandboxHttp) -> Result<String> {
    let resp = http.request("POST", "/session", Some("{}"))?;
    let body = resp.body.trim();
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        PillboxError::runtime(
            ACTION,
            format!("create session: unexpected response: {body}"),
        )
    })?;
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            PillboxError::runtime(ACTION, format!("create session: no id in {body}")).into()
        })
}

/// Drive the session: `POST /session/{id}/prompt_async` with one text part and
/// the model. Async/streaming — the response is `204` and the turn streams on
/// `/event` (read via [`spawn_event_bridge`]). `model` is `provider/modelID`.
pub(crate) fn send_prompt(
    http: &dyn SandboxHttp,
    opencode_session: &str,
    text: &str,
    model: &str,
) -> Result<()> {
    let (provider, model_id) = model.split_once('/').ok_or_else(|| {
        PillboxError::usage(
            "session send",
            format!("--model must be `provider/modelID` (got `{model}`)"),
        )
    })?;
    let body = serde_json::json!({
        "parts": [{ "type": "text", "text": text }],
        "model": { "providerID": provider, "modelID": model_id },
    })
    .to_string();
    let path = format!("/session/{opencode_session}/prompt_async");
    let resp = http.request("POST", &path, Some(&body))?;
    // Any 2xx is success (prompt_async returns 204).
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(PillboxError::runtime(
            "session send",
            format!("opencode prompt failed (HTTP {})", resp.status),
        )
        .into())
    }
}

/// Report a freshly-started server session — `--json` (for orchestrators to
/// capture the id) or the human banner with the watch/send next-steps. Shared by
/// every backend's `run_server` (the bring-up is identical; only the sandbox
/// lifecycle around it differs). Reads the model from the record's server state.
///
/// `run` does **not** auto-send an initial prompt for server agents — the server
/// comes up ready and prompts are driven through `session send` (so the turn is
/// captured by a subscribed `watch`/`subscribe`, not streamed to no one at
/// start). If the user passed a prompt, the send hint pre-fills it.
pub(crate) fn print_started(
    session: &crate::session::Session,
    json: bool,
    pending_prompt: Option<&str>,
) {
    if json {
        println!(
            "{}",
            crate::paths::json_v1(vec![("session", session.to_json_value())])
        );
        return;
    }
    let model = session
        .server
        .as_ref()
        .map(|s| s.model.as_str())
        .unwrap_or("?");
    println!(
        "pillbox: ✓ opencode session `{}` ready ({model}).",
        session.id
    );
    println!(
        "         pillbox session watch {}    # read the stream",
        session.id
    );
    match pending_prompt {
        Some(p) => println!(
            "         pillbox session send {} {p:?}  # send your prompt",
            session.id
        ),
        None => println!(
            "         pillbox session send {} \"…\"  # drive it",
            session.id
        ),
    }
}

/// Stream the server's `/event` SSE into the durable [`SessionLog`] — the
/// `Server`-mode analog of the transcript tailer. The transport's `/event`
/// stream feeds [`drain_sse`] on a thread; the returned handle stops the stream
/// on shutdown (the blocking read can't observe the flag mid-frame). `None` if
/// the stream can't open.
pub(crate) fn spawn_event_bridge(
    http: &dyn SandboxHttp,
    session_id: &str,
    log: SessionLog,
) -> Option<TailerHandle> {
    let stream = http
        .open_stream("/event")
        .map_err(|e| eprintln!("pillbox: warning: couldn't open the opencode event stream: {e:#}"))
        .ok()?;
    let body = stream.body;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let sid = session_id.to_string();
    let join = std::thread::spawn(move || {
        let mut log = log;
        if let Err(e) = drain_sse(body, &sid, &mut log, &stop_thread) {
            eprintln!("pillbox: warning: opencode event stream stopped: {e:#}");
        }
    });
    Some(TailerHandle::from_stopper(stop, stream.stopper, join))
}
