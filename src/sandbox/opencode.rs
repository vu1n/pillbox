//! Server-mode (opencode) sandbox bridge — talk to a headless `opencode serve`
//! running *inside* the sandbox over its HTTP API.
//!
//! opencode is an [`Integration::Server`](crate::agents::Integration) agent: it
//! runs as an HTTP server inside the sandbox and we drive/read it over its API
//! rather than a PTY. Every call here goes through a [`SandboxHttp`] transport,
//! so the bridge is backend-agnostic.
//!
//! - [`serve_args`] / [`serve_env`] — the in-sandbox command and its env.
//! - [`wait_ready`] — poll `GET /api/info` until the server answers.
//! - [`create_session`] — `POST /api/session` (with the model) → the session id.
//! - [`send_prompt`] — `POST /api/session/{id}/prompt` (admits the input; the
//!   turn streams on `/api/event`).
//! - [`spawn_event_bridge`] — `GET /api/event` (SSE) → [`drain_sse`] → durable log.
//!
//! Targets the OpenCode 2 server API (`@opencode/cli`, verified live against
//! 2.0.24): every route lives under `/api/`, the server always requires a
//! password (HTTP basic, user `opencode`), the model is chosen per session, and
//! there is no per-prompt temperature. See docs/opencode-integration.md.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;

use crate::errors::PillboxError;
use crate::events::log::SessionLog;
use crate::events::opencode::drain_sse;
use crate::events::transcripts::TailerHandle;
use crate::sandbox::http::SandboxHttp;

const ACTION: &str = "run (opencode server)";

/// Port the in-sandbox `opencode serve` listens on (guest loopback only; reached
/// by the backend's [`SandboxHttp`] transport).
pub(crate) const SERVE_PORT: u16 = 4096;

/// HTTP basic-auth user OpenCode 2's server expects.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const SERVER_USER: &str = "opencode";

/// The guest server's password. OpenCode 2 refuses to serve without one (it
/// generates a random password when none is set), so the guest launch pins
/// this value through `OPENCODE_SERVER_PASSWORD` and the host transport sends
/// it. It is not a secret: the server binds guest loopback and is reached from
/// the host only through the session's private vsock socket; the microVM is the
/// boundary, not this password.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) const SERVER_PASSWORD: &str = "pillbox-guest-loopback";

/// Default model when `--model` isn't given. `provider/modelID`. OpenCode 2
/// binds the model when the session is created, and the user's config sets no
/// default, so we supply one; override with `pillbox run --agent opencode --model …`.
pub(crate) const DEFAULT_MODEL: &str = "zai-coding-plan/glm-4.5-air";

/// Filename (under the agent home) the in-sandbox `/api/event` capture is
/// appended to — opencode's durable, gateway-free §0 transcript. A guest-side
/// `curl -N /api/event` loop writes raw SSE here; because it lives in the
/// shared/CoW home it persists + is host-readable, so the host drains it
/// (replay + follow) on `watch`/`subscribe` and captures completely even for a
/// late reader. OpenCode documents its live stream as volatile (a slow consumer
/// is dropped, events during a disconnect are missed), which is why the capture
/// is a co-located file rather than a host-side subscription. See
/// [`crate::events::opencode::FollowReader`].
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

/// Guest env for `opencode serve`: the pinned server password, and — when a
/// sampling temperature was requested — an inline config that sets it on the
/// session's model.
///
/// OpenCode 2 has no per-prompt temperature. The only placement verified to
/// reach the provider request is the model's request `body` in config
/// (`providers.<provider>.models.<model>.body.temperature`, merged over the
/// built-in catalog); agent-level `request.body` and the v1 `temperature` /
/// `options.temperature` fields were accepted but never sent (checked against
/// a capturing provider on 2.0.24).
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn serve_env(model: &str, temperature: Option<f64>) -> Result<Vec<(String, String)>> {
    let mut env = vec![(
        "OPENCODE_SERVER_PASSWORD".to_string(),
        SERVER_PASSWORD.to_string(),
    )];
    if let Some(t) = temperature {
        let (provider, model_id) = split_model(model, ACTION)?;
        let config = serde_json::json!({
            "providers": { provider: { "models": { model_id: { "body": { "temperature": t } } } } }
        });
        env.push(("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string()));
    }
    Ok(env)
}

/// `provider/modelID` → `(provider, modelID)`.
fn split_model<'a>(model: &'a str, action: &'static str) -> Result<(&'a str, &'a str)> {
    model.split_once('/').ok_or_else(|| {
        PillboxError::usage(
            action,
            format!("--model must be `provider/modelID` (got `{model}`)"),
        )
        .into()
    })
}

/// Poll `GET /api/info` until the server answers `200` (the migration + boot
/// can take a few seconds), bounded so a dead server fails loud instead of hanging.
pub(crate) fn wait_ready(http: &dyn SandboxHttp) -> Result<()> {
    for _ in 0..60 {
        if let Ok(resp) = http.request("GET", "/api/info", None) {
            if resp.status == 200 {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(PillboxError::runtime(ACTION, "opencode server didn't become ready in 30s").into())
}

/// `POST /api/session` with the model → the new session id (`ses_…`, under
/// `data.id`). `model` is `provider/modelID`.
pub(crate) fn create_session(http: &dyn SandboxHttp, model: &str) -> Result<String> {
    let (provider, model_id) = split_model(model, ACTION)?;
    let body = serde_json::json!({ "model": { "providerID": provider, "id": model_id } });
    let resp = http.request("POST", "/api/session", Some(&body.to_string()))?;
    let raw = resp.body.trim();
    if !(200..300).contains(&resp.status) {
        return Err(PillboxError::runtime(
            ACTION,
            format!("create session failed (HTTP {}): {raw}", resp.status),
        )
        .into());
    }
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
        PillboxError::runtime(
            ACTION,
            format!("create session: unexpected response: {raw}"),
        )
    })?;
    value
        .pointer("/data/id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            PillboxError::runtime(ACTION, format!("create session: no data.id in {raw}")).into()
        })
}

/// Drive the session: `POST /api/session/{id}/prompt` with the text. OpenCode
/// admits the input durably and returns it (`200`); the turn itself streams on
/// `/api/event` (read via [`spawn_event_bridge`] or the guest capture).
pub(crate) fn send_prompt(
    http: &dyn SandboxHttp,
    opencode_session: &str,
    text: &str,
) -> Result<()> {
    let body = serde_json::json!({ "text": text }).to_string();
    let path = format!("/api/session/{opencode_session}/prompt");
    let resp = http.request("POST", &path, Some(&body))?;
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(PillboxError::runtime(
            "session send",
            format!(
                "opencode prompt failed (HTTP {}): {}",
                resp.status,
                resp.body.trim()
            ),
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
        crate::session::print_started_json(session);
        return;
    }
    let model = session
        .server
        .as_ref()
        .map(|s| s.model.as_str())
        .unwrap_or("?");
    println!(
        "pillbox: ✓ {} session `{}` ready ({model}).",
        session.agent_id, session.id
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

/// Stream the server's `/api/event` SSE into the durable [`SessionLog`] — the
/// `Server`-mode analog of the transcript tailer. The transport's `/api/event`
/// stream feeds [`drain_sse`] on a thread; the returned handle stops the stream
/// on shutdown (the blocking read can't observe the flag mid-frame). `None` if
/// the stream can't open.
pub(crate) fn spawn_event_bridge(
    http: &dyn SandboxHttp,
    session_id: &str,
    log: SessionLog,
) -> Option<TailerHandle> {
    let stream = http
        .open_stream("/api/event")
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
