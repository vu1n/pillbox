//! Server-mode (opencode) sandbox bridge — talk to a headless `opencode serve`
//! running *inside* the sandbox over its HTTP API.
//!
//! opencode is an [`Integration::Server`](crate::agents::Integration) agent: it
//! runs as an HTTP server inside the sandbox and we drive/read it over its API
//! rather than a PTY. Every call here reaches that server by running `curl`
//! against `127.0.0.1:<port>` *inside* the sandbox via a [`SandboxExec`] — so
//! the bridge is transport-agnostic: docker supplies `docker exec`, libkrun a
//! vsock exec channel, with no port publishing either way.
//!
//! - [`serve_args`] — the in-sandbox command (`opencode serve …`).
//! - [`wait_ready`] — poll `/doc` until the server answers.
//! - [`create_session`] — `POST /session` → the opencode session id.
//! - [`send_prompt`] — `POST /session/{id}/prompt_async` (the streaming drive).
//! - [`spawn_event_bridge`] — `curl -N /event` → [`drain_sse`] → durable log.
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
use crate::sandbox::exec::SandboxExec;

const ACTION: &str = "run (opencode server)";

/// Port the in-sandbox `opencode serve` listens on (localhost-only; reached by
/// `curl` inside the same sandbox, so no auth/publish needed).
pub(crate) const SERVE_PORT: u16 = 4096;

/// Default model when `--model` isn't given. `provider/modelID`. opencode's
/// `prompt_async` requires a model and the user's config sets no default, so we
/// supply one; override with `pillbox run --agent opencode --model …`.
pub(crate) const DEFAULT_MODEL: &str = "zai-coding-plan/glm-4.5-air";

fn base_url() -> String {
    format!("http://127.0.0.1:{SERVE_PORT}")
}

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

/// Run a one-shot `curl` in the sandbox and capture its stdout. stderr handling
/// is the transport's call; a non-zero curl exit surfaces via an empty/short
/// body the callers validate.
fn exec_curl(exec: &dyn SandboxExec, curl_args: &[&str]) -> Result<String> {
    // `-s` (not `-sS`): fully silent. `wait_ready` polls before the server
    // binds, and those expected connect failures shouldn't spew to stderr;
    // real failures surface via the empty body / non-2xx code callers check.
    let mut argv = vec!["curl", "-s"];
    argv.extend_from_slice(curl_args);
    Ok(exec.exec(&argv)?.read_to_string())
}

/// Poll `GET /doc` until the server answers `200` (the migration + boot can take
/// a few seconds), bounded so a dead server fails loud instead of hanging.
pub(crate) fn wait_ready(exec: &dyn SandboxExec) -> Result<()> {
    let url = format!("{}/doc", base_url());
    for _ in 0..60 {
        let code = exec_curl(exec, &["-o", "/dev/null", "-w", "%{http_code}", &url])?;
        if code.trim() == "200" {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(PillboxError::runtime(ACTION, "opencode server didn't become ready in 30s").into())
}

/// `POST /session` → the new opencode session id (`ses_…`).
pub(crate) fn create_session(exec: &dyn SandboxExec) -> Result<String> {
    let url = format!("{}/session", base_url());
    let body = exec_curl(
        exec,
        &[
            "-X",
            "POST",
            &url,
            "-H",
            "content-type: application/json",
            "-d",
            "{}",
        ],
    )?;
    let value: serde_json::Value = serde_json::from_str(body.trim()).map_err(|_| {
        PillboxError::runtime(
            ACTION,
            format!("create session: unexpected response: {}", body.trim()),
        )
    })?;
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            PillboxError::runtime(ACTION, format!("create session: no id in {}", body.trim()))
                .into()
        })
}

/// Drive the session: `POST /session/{id}/prompt_async` with one text part and
/// the model. Async/streaming — the response is `204` and the turn streams on
/// `/event` (read via [`spawn_event_bridge`]). `model` is `provider/modelID`.
pub(crate) fn send_prompt(
    exec: &dyn SandboxExec,
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
    let url = format!("{}/session/{opencode_session}/prompt_async", base_url());
    let code = exec_curl(
        exec,
        &[
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            &url,
            "-H",
            "content-type: application/json",
            "-d",
            &body,
        ],
    )?;
    // Any 2xx is success (prompt_async returns 204).
    if code.trim().starts_with('2') {
        Ok(())
    } else {
        Err(PillboxError::runtime(
            "session send",
            format!("opencode prompt failed (HTTP {})", code.trim()),
        )
        .into())
    }
}

/// Stream the server's `/event` SSE into the durable [`SessionLog`] — the
/// `Server`-mode analog of the transcript tailer. A `curl -N /event` in the
/// sandbox feeds [`drain_sse`] on a thread; the returned handle stops the exec
/// on shutdown (the blocking read can't observe the flag mid-frame). `None` if
/// the exec can't spawn.
pub(crate) fn spawn_event_bridge(
    exec: &dyn SandboxExec,
    session_id: &str,
    log: SessionLog,
) -> Option<TailerHandle> {
    let url = format!("{}/event", base_url());
    let child = exec
        .exec(&["curl", "-sN", &url])
        .map_err(|e| eprintln!("pillbox: warning: couldn't open the opencode event stream: {e:#}"))
        .ok()?;
    let stdout = child.stdout;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let sid = session_id.to_string();
    let join = std::thread::spawn(move || {
        let mut log = log;
        if let Err(e) = drain_sse(stdout, &sid, &mut log, &stop_thread) {
            eprintln!("pillbox: warning: opencode event stream stopped: {e:#}");
        }
    });
    Some(TailerHandle::from_stopper(stop, child.stopper, join))
}
