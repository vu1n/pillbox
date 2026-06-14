//! Lifecycle events stream — JSONL append to `<pillbox>/events.jsonl`.
//!
//! ## Event taxonomy
//!
//! Four lifecycle events, all OTel-shaped. The `emitter` attribute
//! (`"host"` / `"sandbox"`) on every event disambiguates which side
//! originated the line — `session.started` is the canonical case
//! (host emits when the sandbox handshake completes; sandbox emits
//! when it's running the agent), and the delta gives cold-start
//! latency.
//!
//! | Event              | Emitted by                | When                                        |
//! |--------------------|---------------------------|---------------------------------------------|
//! | `session.started`  | host pillbox + sandbox    | Host: sandbox handshake; Sandbox: self-init |
//! | `session.completed`| sandbox (`session done`)  | Agent finished successfully                 |
//! | `session.failed`   | sandbox (`session done`)  | Agent exited non-zero / errored             |
//! | `session.dropped`  | host pillbox              | `session rm` torn the sandbox down          |
//!
//! Sandbox-side events: a wrapper around the agent calls
//! `pillbox session done <id> --status ok|failed` after the agent
//! exits, and the sandbox-side pillbox emits the event via whichever
//! sink the env exposes (`PILLBOX_EVENTS_WEBHOOK`,
//! `OTEL_EXPORTER_OTLP_ENDPOINT`). For detached runs without a
//! configured sink, the host won't see the terminal event — documented
//! limitation, the trade-off for avoiding a daemon.
//!
//! ## Field shape (per JSONL line)
//!
//! ```jsonc
//! {
//!   "version": 1,                          // bump on breaking field-set change
//!   "event": "session.completed",
//!   "emitter": "sandbox",                  // "host" or "sandbox" — disambiguates
//!                                          //  the dual session.started lines
//!   "session_id": "abc123def456",          // → OTel span_id
//!   "parent_session_id": "789...",         // → OTel parent_span_id (forks);
//!                                          //  only set on session.started
//!   "started_at": "2026-05-23T13:37:00Z",  // → OTel span.start_time
//!   "ended_at":   "2026-05-23T13:42:11Z",  // → OTel span.end_time (terminal only)
//!   "agent_id": "claude",
//!   "backend": "docker",
//!   "label": null,
//!   "startup_ms": 421,
//!   "startup_stages": [
//!     { "name": "docker_preflight", "duration_ms": 31 },
//!     { "name": "container_start", "duration_ms": 390 }
//!   ],
//!   // Terminal-event-only fields (null on started / dropped):
//!   "status": "ok",                        // → OTel status.code ("ok" | "error")
//!   "reason": null,                        // free-text on failed
//!   "exit_code": 0,
//!   "trace_path": "rustic://snapshot/.../trace.jsonl"
//! }
//! ```
//!
//! ## Sinks
//!
//! Three sinks, each in its own submodule
//! ([`jsonl`], [`webhook`], [`otel`]), all driven by the same
//! [`emit_session_event`] call site. Each is best-effort
//! independently — a failed webhook POST doesn't prevent the JSONL
//! append from succeeding.
//!
//! - **JSONL** — appends to `<pillbox>/events.jsonl` (0600). Always
//!   active on the host. Sandbox-side pillbox also writes here but the
//!   file is ephemeral with the sandbox.
//! - **Webhook** — POSTs each event to `--events-webhook URL` (or
//!   `$PILLBOX_EVENTS_WEBHOOK`). Used to ferry sandbox-side events
//!   back to the orchestrator without pillbox running a daemon.
//! - **OTel** — emits one OTLP log record per event AND (sandbox-
//!   side, terminal-only) one OTLP span per session to whichever
//!   collector `$OTEL_EXPORTER_OTLP_ENDPOINT` points at. Default
//!   transport is HTTP/protobuf via the blocking reqwest client (no
//!   tokio runtime drag — matches the webhook sink's sync model).
//!   Optional gRPC behind the `otel-grpc` cargo feature. Span
//!   emission is gated on `PILLBOX_SESSION_STARTED_AT` being set by
//!   the wrapper so `span.start_time` is meaningful — without it the
//!   log record still ships, the span doesn't.
//!
//! Best-effort writes: a failed sink emit logs a warning and
//! continues. The agent run is more important than the event log; the
//! orchestrator can tolerate a missed event.

use std::{
    fs,
    io::{self, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::OnceLock,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::pillbox::Pillbox;
use crate::session::{self, Session};
use crate::startup::StartupMetrics;

// codex-serve is libkrun-only (docker rejects it), so the mapper + NDJSON drain
// are consumed only under that feature (and by tests); allow dead-code otherwise.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) mod codex_serve;
mod jsonl;
pub(crate) mod log;
pub(crate) mod opencode;
mod otel;
pub(crate) mod sink;
pub(crate) mod source;
pub(crate) mod status;
pub(crate) mod transcripts;
mod webhook;

pub(crate) use otel::genai::{
    emit_call_span as emit_genai_call_span, CallSpan as GenAiCallSpan, GenAiUsage,
};

/// Wire format of a server-mode agent's §0 capture file — the axis that selects
/// the drain function. opencode's `/event` stream is SSE; the codex app-server
/// bridge writes newline-delimited JSON. Carried on the agent's
/// [`ServerProfile`](crate::agents::ServerProfile) so the drain sites
/// (`session watch`/`subscribe`/`ingest`) dispatch on data, not on the agent id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) enum EventsFormat {
    Sse,
    Ndjson,
}

#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
impl EventsFormat {
    /// Stable token for passing the format over argv to the detached §0 producer.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EventsFormat::Sse => "sse",
            EventsFormat::Ndjson => "ndjson",
        }
    }

    pub(crate) fn from_token(s: &str) -> Option<Self> {
        match s {
            "sse" => Some(EventsFormat::Sse),
            "ndjson" => Some(EventsFormat::Ndjson),
            _ => None,
        }
    }
}

/// Drain a server agent's persisted capture (`reader`) into its durable log,
/// dispatching on the capture [`EventsFormat`] — the single home for the
/// format→drain mapping, shared by the live tailer and the post-hoc `ingest`.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn drain_server_capture<R: std::io::Read>(
    format: EventsFormat,
    reader: R,
    session_id: &str,
    log: &mut log::SessionLog,
    stop: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<usize> {
    match format {
        EventsFormat::Sse => opencode::drain_sse(reader, session_id, log, stop),
        EventsFormat::Ndjson => codex_serve::drain_ndjson(reader, session_id, log, stop),
    }
}

/// Emit the root `session` span for a local-docker foreground run from
/// the host, at session start (see [`otel::emit_local_root_span`] for
/// why up-front and why the host owns it). gen_ai + transcript child
/// spans nest under it by shared trace/span id. No-op when no OTLP
/// traces endpoint is configured.
pub(crate) fn emit_local_session_span(session_id: &str, start: std::time::SystemTime) {
    otel::emit_local_root_span(session_id, start);
}

/// Whether an OTLP traces endpoint is configured (either the signal-
/// specific `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or the base
/// `OTEL_EXPORTER_OTLP_ENDPOINT`). The local-docker backend gates the
/// host-side transcript tailer + root span on this so a plain run with
/// no collector doesn't spawn a watcher thread for nothing.
pub(crate) fn otlp_traces_configured() -> bool {
    otel::resolve_signal_endpoint("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "v1/traces").is_some()
}

/// Default OTLP/HTTP port the Raindrop Workshop daemon listens on.
const WORKSHOP_ENDPOINT: &str = "http://localhost:5899";

/// One-line stderr nudge when a Raindrop Workshop install is present but
/// no OTLP endpoint is configured — otherwise a local `pillbox run`
/// silently streams nothing and the user is left wondering why Workshop
/// is empty.
///
/// Detection is a filesystem `stat` of `~/.raindrop` (the installer's
/// data dir), deliberately NOT a TCP probe of the daemon port: a probe
/// risks adding startup latency or hanging when nothing is listening,
/// whereas a `stat` is instant and costs nothing for users who never
/// installed Raindrop. The hint self-resolves the moment `OTEL_*` is
/// set. Caller restricts this to local runs (Workshop's `localhost`
/// endpoint isn't reachable from a remote sandbox).
pub(crate) fn hint_workshop_if_unconfigured() {
    if otlp_traces_configured() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    if !std::path::Path::new(&home).join(".raindrop").is_dir() {
        return;
    }
    eprintln!(
        "pillbox: note: Raindrop Workshop is installed but telemetry is off — \
         this run streams nothing.\n         \
         Set OTEL_EXPORTER_OTLP_ENDPOINT={WORKSHOP_ENDPOINT} to stream it live."
    );
}

/// Filename under `<pillbox>/state_dir/`. Append-only JSONL.
pub(crate) const EVENTS_FILE: &str = "events.jsonl";

/// Per-event schema version. Bumped on a breaking field-set change so
/// consumers can pin against it (`select(.version == 1)`). Mirrors the
/// discipline `paths::json_v1` applies to one-shot `--json` payloads;
/// stamped per-line here because JSONL has no envelope to carry it.
const EVENT_SCHEMA_VERSION: u32 = 1;

/// Polling interval for `--follow` mode. 200ms is fast enough for
/// human-paced session lifecycles and slow enough not to spin CPU.
/// Real PR 2 will use inotify / kqueue.
const FOLLOW_POLL_MS: u64 = 200;

/// Per-sink network budget. A slow collector / webhook shouldn't
/// dominate a session's runtime: a full lifecycle is ~3 emits, so
/// worst case a dead endpoint adds ~6s to a run. Both webhook and
/// OTel sinks honor this; the OTLP-standard
/// `OTEL_EXPORTER_OTLP_TIMEOUT` env overrides it for OTel.
pub(super) const EVENTS_SINK_TIMEOUT: Duration = Duration::from_secs(2);

/// The managed §0 placement for `session_id` when the managed tier is on: the
/// per-session Durable Object endpoint + an optional actor token. `None` → use
/// the local file-backed placement. The one home for "where is the managed DO
/// for this session", shared by the write-side ([`sink::open_event_log`]) and
/// read-side ([`source::open_event_source`]) factories so they can't drift on
/// endpoint shape or token handling. The token is `None` when unset or empty
/// (the DO allows anonymous reads; the write side maps `None` → `""`, which it
/// fails closed on).
pub(super) fn managed_endpoint(session_id: &str) -> Option<(String, Option<String>)> {
    let base = std::env::var("PILLBOX_MANAGED_DO_URL").ok()?;
    let token = std::env::var("PILLBOX_ACTOR_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let endpoint = format!(
        "{}/agents/session-gateway/{session_id}",
        base.trim_end_matches('/')
    );
    Some((endpoint, token))
}

/// One lifecycle event variant. Terminal events (`SessionCompleted` /
/// `SessionFailed`) carry the variant-specific payload inline so
/// [`build_attributes`] can be exhaustive at compile time. Lost
/// `Copy` (vs. the spike's unit-only enum) because the variants now
/// own `String`s — accept the move-by-value cost since emission is
/// one-shot per call site.
///
/// `Session` prefix on every variant is intentional — events are
/// scoped to sessions today, and the prefix matches the wire name
/// (`session.started` etc.). Clippy's `enum_variant_names` lint
/// suggests trimming the prefix, but doing so would decouple the
/// variant name from the on-wire `event` string.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub(crate) enum EventType {
    SessionStarted {
        /// Optional reference to the parent session id when this run
        /// was forked from another. Maps to OTel `parent_span_id` and
        /// the wire field `parent_session_id`; `None` for root
        /// sessions.
        parent_session_id: Option<String>,
        /// Host-side launch timing, present on host-emitted started
        /// events when the backend can measure it. Sandbox-side
        /// `session started` emits `None` because it only knows its own
        /// wall-clock start.
        startup: Option<StartupMetrics>,
    },
    SessionCompleted {
        exit_code: Option<i32>,
        trace_path: Option<String>,
        /// Rustic snapshot handle of the agent's result workspace,
        /// pushed by the in-sandbox wrapper after the agent exits.
        /// Consumers correlate with `base_snapshot` (on the session
        /// record + the host's `session.started` event) to compute
        /// the fork's diff. `session pull <id>` rehydrates from this
        /// handle.
        result_snapshot: Option<String>,
    },
    SessionFailed {
        reason: String,
        exit_code: Option<i32>,
        trace_path: Option<String>,
        result_snapshot: Option<String>,
    },
    SessionDropped,
}

impl EventType {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            EventType::SessionStarted { .. } => "session.started",
            EventType::SessionCompleted { .. } => "session.completed",
            EventType::SessionFailed { .. } => "session.failed",
            EventType::SessionDropped => "session.dropped",
        }
    }

    /// `ok` / `error` per OTel `status.code` semantics. Started / dropped
    /// are non-terminal — `unset` per the OTel default.
    pub(crate) fn status_code(&self) -> &'static str {
        match self {
            EventType::SessionCompleted { .. } => "ok",
            EventType::SessionFailed { .. } => "error",
            _ => "unset",
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            EventType::SessionCompleted { .. } | EventType::SessionFailed { .. }
        )
    }
}

/// Env var the in-sandbox wrapper script sets so the sandbox-side
/// pillbox binary can self-identify on event emission. Host pillbox
/// never sets it.
///
/// Note: this is an observability tag, not an access-control signal.
/// Anything that can write the process env can set this, so consumers
/// must not treat `emitter == "host"` as a trust boundary.
const SANDBOX_SIDE_ENV: &str = "PILLBOX_SANDBOX_SIDE";

/// Env var the host's `pillbox run --parent <id>` sets so both the
/// host's own `session.started` emit and the sandbox-side
/// `session started` CLI (via the helper's bash export) can pick up
/// the parent reference without re-threading through call signatures.
pub(crate) const PARENT_SESSION_ID_ENV: &str = "PILLBOX_PARENT_SESSION_ID";

/// Env var the wrapper script captures via `date -u -Iseconds` so the
/// sandbox-side `session started` and `session done` invocations
/// (different processes) read the SAME timestamp for the session's
/// start. Used by `started_at` on the started event AND
/// `span.start_time` on the terminal event's span — pinning both to
/// one wall-clock read avoids microsecond skew between the two
/// emitter paths.
///
/// Like the other PILLBOX_* env vars, this is observability tagging
/// only — anything that can write the process env can backdate the
/// span. Consumers must not treat `span.start_time` as a trust
/// boundary; it's a self-reported timestamp from the agent's
/// execution context.
pub(crate) const SESSION_STARTED_AT_ENV: &str = "PILLBOX_SESSION_STARTED_AT";

/// Read [`PARENT_SESSION_ID_ENV`] and normalize empty → None. Shared
/// chokepoint so the host and sandbox paths can't drift on env-name
/// or empty-string handling.
pub(crate) fn parent_session_id_from_env() -> Option<String> {
    std::env::var(PARENT_SESSION_ID_ENV)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Read [`SESSION_STARTED_AT_ENV`] and normalize empty → None.
pub(crate) fn session_started_at_from_env() -> Option<String> {
    std::env::var(SESSION_STARTED_AT_ENV)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Which side of the host/sandbox split emitted this event. Lets
/// consumers tell apart the two `session.started` lines (host's
/// "I saw the sandbox come up" vs. sandbox's "I'm running the
/// agent now") — and any other event that ends up emitted from
/// both sides — without inventing distinct event names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Emitter {
    Host,
    Sandbox,
}

impl Emitter {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Emitter::Host => "host",
            Emitter::Sandbox => "sandbox",
        }
    }
}

/// Process-level emitter, detected once on first call from
/// [`SANDBOX_SIDE_ENV`]. Cached so per-emit cost is one atomic load.
pub(super) fn current_emitter() -> Emitter {
    static DETECTED: OnceLock<Emitter> = OnceLock::new();
    *DETECTED.get_or_init(|| match std::env::var(SANDBOX_SIDE_ENV).ok().as_deref() {
        Some(v) if !v.is_empty() => Emitter::Sandbox,
        _ => Emitter::Host,
    })
}

/// One attribute value. The JSONL and OTel sinks each map this onto
/// their wire format; consolidating to a single type means a new
/// event field is a one-line addition to [`build_attributes`] instead
/// of edits coordinated across the JSONL renderer + OTel record
/// filler. `String` and `i64` are the only shapes the current event
/// taxonomy uses; widening is a one-variant change.
#[derive(Debug)]
pub(super) enum AttrValue {
    Str(String),
    Int(i64),
    Json(serde_json::Value),
}

/// Emit one event for a session lifecycle transition. `session` is
/// optional — `Some` when emitted from the host (full record), `None`
/// when emitted from inside a sandbox where only the id is known.
/// Missing fields render as JSON nulls in the payload (not empty
/// strings); consumers correlate sandbox-side events with the host's
/// `session.started` via the shared `session_id`.
///
/// Routes through every configured sink (JSONL always; webhook + OTel
/// if env / flags set). Never panics; per-sink errors are logged to
/// stderr so a broken sink doesn't kill the run.
pub(crate) fn emit_session_event(
    pb: &Pillbox,
    ty: EventType,
    session_id: &str,
    session: Option<&Session>,
) {
    // Compute attrs ONCE so all sinks render from the same snapshot —
    // otherwise per-sink `now_rfc3339()` calls would produce
    // `ended_at` values differing by microseconds and downstream
    // consumers correlating across sinks would see false drift.
    let attrs = build_attributes(&ty, session_id, session);
    let payload = jsonl::render(&attrs);
    let name = ty.as_str();
    // JSONL is the always-on sink. Failures fall through to a warning;
    // we don't want a missing state dir to abort the agent run.
    warn_on_sink_error("jsonl", name, jsonl::sink_emit(pb, &payload));
    // Webhook sink — only fires if the env var is set. Sandbox-side
    // pillbox uses this to ferry terminal events back to whoever is
    // listening (typically the orchestrator).
    if let Ok(url) = std::env::var("PILLBOX_EVENTS_WEBHOOK") {
        if !url.is_empty() {
            warn_on_sink_error("webhook", name, webhook::sink_emit(&url, &payload));
        }
    }
    // OTel span sink — sandbox-only, terminal-only, requires
    // SESSION_STARTED_AT_ENV. Fires BEFORE the log sink so a
    // synchronous exporter failure surfaces as a warning before the
    // log line claims success. Borrows `attrs` (log sink consumes).
    warn_on_sink_error(
        "otel-span",
        name,
        otel::span_sink_emit(&ty, session_id, &attrs, current_emitter()),
    );
    // OTel log sink — fires if OTEL_EXPORTER_OTLP_ENDPOINT is set.
    // Consumes `attrs` (last sink to touch them).
    warn_on_sink_error("otel", name, otel::log_sink_emit(&ty, attrs));
}

/// One-place warning formatter so each sink only adds a
/// `warn_on_sink_error("name", …)` line, not another bespoke
/// `if let Err(e)` block. Per-sink failures stay independent — a slow
/// webhook can't suppress the JSONL append, etc.
fn warn_on_sink_error(sink: &str, event: &str, result: Result<()>) {
    if let Err(e) = result {
        eprintln!("pillbox: warning: {sink} sink failed for {event}: {e}");
    }
}

/// Single source of truth for "what attributes does this event
/// carry." Both the JSONL renderer and the OTel record filler
/// consume from this list — adding a new field is a one-line edit
/// here, not three coordinated changes.
///
/// `None` values exist (vs. omission) because the JSONL format does
/// distinguish present-but-null from absent — every event line has
/// the same key set so consumers can do positional decoding. The
/// OTel sink filters out `None` since attribute bags have no notion
/// of "present with null value."
///
/// Ordering is stable + matches the historical JSONL layout (version
/// first so a consumer can branch on schema before parsing the
/// rest); changing it is a v=2 schema bump.
pub(crate) fn build_attributes(
    ty: &EventType,
    session_id: &str,
    session: Option<&Session>,
) -> Vec<(&'static str, Option<AttrValue>)> {
    let ended_at = (ty.is_terminal() || matches!(ty, EventType::SessionDropped))
        .then(|| AttrValue::Str(session::now_rfc3339()));
    let (parent_session_id, startup) = match ty {
        EventType::SessionStarted {
            parent_session_id,
            startup,
        } => (parent_session_id.clone(), startup.clone()),
        _ => (None, None),
    };
    let (reason, exit_code, trace_path, result_snapshot) = match ty {
        EventType::SessionCompleted {
            exit_code,
            trace_path,
            result_snapshot,
        } => (
            None,
            *exit_code,
            trace_path.clone(),
            result_snapshot.clone(),
        ),
        EventType::SessionFailed {
            reason,
            exit_code,
            trace_path,
            result_snapshot,
        } => (
            Some(reason.clone()),
            *exit_code,
            trace_path.clone(),
            result_snapshot.clone(),
        ),
        _ => (None, None, None, None),
    };
    // Session-derived fields collapse Some("") and None to None — an
    // empty `agent_id` would be a lie ("the agent's name is the
    // empty string"), not "we don't know the agent".
    let s_str = |f: fn(&Session) -> &str| -> Option<AttrValue> {
        session
            .map(f)
            .filter(|s| !s.is_empty())
            .map(|s| AttrValue::Str(s.to_string()))
    };
    let s_opt = |f: fn(&Session) -> Option<&str>| -> Option<AttrValue> {
        session.and_then(f).map(|s| AttrValue::Str(s.to_string()))
    };
    vec![
        ("version", Some(AttrValue::Int(EVENT_SCHEMA_VERSION as i64))),
        ("event", Some(AttrValue::Str(ty.as_str().to_string()))),
        (
            "emitter",
            Some(AttrValue::Str(current_emitter().as_str().to_string())),
        ),
        ("session_id", Some(AttrValue::Str(session_id.to_string()))),
        ("parent_session_id", parent_session_id.map(AttrValue::Str)),
        ("started_at", s_str(|s| &s.started_at)),
        ("ended_at", ended_at),
        ("agent_id", s_str(|s| &s.agent_id)),
        ("backend", s_str(|s| &s.backend)),
        ("label", s_opt(|s| s.label.as_deref())),
        ("status", Some(AttrValue::Str(ty.status_code().to_string()))),
        ("reason", reason.map(AttrValue::Str)),
        ("exit_code", exit_code.map(|c| AttrValue::Int(c as i64))),
        ("trace_path", trace_path.map(AttrValue::Str)),
        ("result_snapshot", result_snapshot.map(AttrValue::Str)),
        ("base_snapshot", s_opt(|s| s.base_snapshot.as_deref())),
        (
            "startup_ms",
            startup.as_ref().map(|m| AttrValue::Int(m.total_ms)),
        ),
        (
            "startup_stages",
            startup.map(|m| AttrValue::Json(m.stages_json())),
        ),
    ]
}

pub(crate) fn events_path(pb: &Pillbox) -> PathBuf {
    pb.state_dir.join(EVENTS_FILE)
}

/// Implementation of `pillbox session events [--follow] [--json]`.
///
/// `--json` is currently a no-op — every event is already JSONL —
/// but the flag is reserved so PR 2 can add a human-readable default
/// mode without breaking the orchestrator's `--json` callers.
pub(crate) fn dispatch_events(resolved: &Pillbox, follow: bool, _json: bool) -> Result<()> {
    let path = events_path(resolved);
    // Print existing events first (so `--follow` includes history, not
    // just new lines — useful when an orchestrator starts mid-loop).
    // Stream via `io::copy` instead of slurping into a `String`: a
    // long-lived pillbox can accumulate megabytes of history in PR 2 /
    // PR 3 once `session.completed` + per-tool-call events arrive, and
    // we don't want a 100MB allocation just to print history once.
    let mut last_size: u64 = 0;
    if path.exists() {
        let mut file = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut stdout = io::stdout();
        // `io::copy` returns the exact byte count it transferred. Use
        // that as the tail's resume point instead of trusting a
        // separately-stat'd size: writes between the stat and the copy
        // would cause us to either miss bytes or print them twice.
        last_size = io::copy(&mut file, &mut stdout)
            .with_context(|| format!("stream {} to stdout", path.display()))?;
        stdout.flush().ok();
    }
    if !follow {
        return Ok(());
    }
    // Naive polling tail. Honest about the choice: fine for human-paced
    // session lifecycles. Real PR 2 will use inotify / kqueue.
    loop {
        thread::sleep(Duration::from_millis(FOLLOW_POLL_MS));
        let size = match path.metadata() {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if size > last_size {
            let mut file =
                fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
            file.seek(SeekFrom::Start(last_size))
                .with_context(|| "seek events file")?;
            let mut stdout = io::stdout();
            let copied =
                io::copy(&mut file, &mut stdout).with_context(|| "stream events to stdout")?;
            stdout.flush().ok();
            // Advance by the actual byte count copied rather than the
            // pre-copy `size` stat: a concurrent emit between the stat
            // and the copy would otherwise either skip the new bytes
            // (advance past them) or replay them on the next poll.
            last_size += copied;
        } else if size < last_size {
            // File rotated / truncated externally; re-read from start.
            last_size = 0;
        }
    }
}

/// Read-side webhook export — the fan-out the architecture review prescribes:
/// a CONSUMER of the per-session log, off the producer's append path, so a slow
/// webhook can't stall the agent. Tails `log` and POSTs notification-worthy
/// events to `url` as JSON. Today that's `AttentionRequired` ("the agent needs
/// you") — the rich signal that only lives on the per-session log; lifecycle
/// events (started/completed/…) already reach the webhook via
/// [`emit_session_event`], so this doesn't duplicate them.
///
/// Starts **live** (from the log's current head — a notification channel
/// shouldn't replay past signals). Runs on a detached thread for the caller's
/// (the gateway's) lifetime; process exit reaps it. Best-effort + loud.
pub(crate) fn spawn_webhook_log_exporter(log: log::SessionLog, url: String) {
    use crate::contract::Payload;
    use std::sync::atomic::AtomicBool;

    let from = log.last_seq() + 1;
    std::thread::spawn(move || {
        let never = AtomicBool::new(false);
        let _ = log.subscribe(from, &never, |event| {
            if matches!(event.payload, Payload::AttentionRequired(_)) {
                if let Ok(json) = serde_json::to_string(event) {
                    warn_on_sink_error(
                        "webhook-export",
                        "attention_required",
                        webhook::sink_emit(&url, &json),
                    );
                }
            }
            true // never stops on its own; the process owns the lifetime
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::test_util::with_isolated_home;

    #[test]
    fn emit_appends_jsonl_line_for_all_event_types() {
        with_isolated_home("events-emit-all", || {
            let pb = pillbox::global();
            let s = Session::test_fixture();
            emit_session_event(
                &pb,
                EventType::SessionStarted {
                    parent_session_id: Some("parent12345".into()),
                    startup: Some(crate::startup::StartupMetrics {
                        total_ms: 42,
                        stages: vec![crate::startup::StartupStage {
                            name: "container_start".into(),
                            duration_ms: 42,
                        }],
                    }),
                },
                &s.id,
                Some(&s),
            );
            emit_session_event(
                &pb,
                EventType::SessionCompleted {
                    exit_code: Some(0),
                    trace_path: Some("rustic://x".into()),
                    result_snapshot: Some("snap-abc".into()),
                },
                &s.id,
                Some(&s),
            );
            emit_session_event(
                &pb,
                EventType::SessionFailed {
                    reason: "agent panic".into(),
                    exit_code: Some(42),
                    trace_path: None,
                    result_snapshot: None,
                },
                &s.id,
                Some(&s),
            );
            emit_session_event(&pb, EventType::SessionDropped, &s.id, Some(&s));
            let content = fs::read_to_string(events_path(&pb)).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            assert_eq!(lines.len(), 4);

            let started: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(started["event"], "session.started");
            assert_eq!(started["session_id"], "abc123def456");
            assert_eq!(started["parent_session_id"], "parent12345");
            assert_eq!(started["ended_at"], serde_json::Value::Null);
            assert_eq!(started["status"], "unset");
            assert_eq!(started["version"], EVENT_SCHEMA_VERSION);
            assert_eq!(started["startup_ms"], 42);
            assert_eq!(started["startup_stages"][0]["name"], "container_start");
            assert_eq!(started["startup_stages"][0]["duration_ms"], 42);
            // emitter is always present; defaults to "host" outside
            // a sandbox-side process. Tests run on the host side.
            assert_eq!(started["emitter"], "host");

            let completed: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
            assert_eq!(completed["event"], "session.completed");
            assert_eq!(completed["status"], "ok");
            assert_eq!(completed["exit_code"], 0);
            assert_eq!(completed["trace_path"], "rustic://x");
            assert!(!completed["ended_at"].is_null());

            let failed: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
            assert_eq!(failed["event"], "session.failed");
            assert_eq!(failed["status"], "error");
            assert_eq!(failed["reason"], "agent panic");
            assert_eq!(failed["exit_code"], 42);
            assert!(failed["trace_path"].is_null());

            let dropped: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
            assert_eq!(dropped["event"], "session.dropped");
            assert!(!dropped["ended_at"].is_null());
        });
    }

    #[test]
    fn emitter_wire_strings_are_stable() {
        // The "host" / "sandbox" strings are the wire contract for
        // the `emitter` event attribute; consumers branch on them.
        // Pin them here so a typo refactor on `Emitter::as_str`
        // surfaces as a test failure rather than a silent break.
        assert_eq!(Emitter::Host.as_str(), "host");
        assert_eq!(Emitter::Sandbox.as_str(), "sandbox");
    }

    #[test]
    fn jsonl_and_otel_share_the_same_field_set() {
        // Single source of truth: both sinks consume `build_attributes`,
        // so the JSONL object keys are exactly the attribute keys. This
        // test pins that the JSONL renderer doesn't accidentally drop
        // or rename a field — adding one to `build_attributes` is the
        // only way to grow the set.
        let s = Session::test_fixture();
        let ty = EventType::SessionFailed {
            reason: "x".into(),
            exit_code: Some(1),
            trace_path: Some("y".into()),
            result_snapshot: Some("z".into()),
        };
        let attrs = build_attributes(&ty, &s.id, Some(&s));
        let raw = jsonl::render(&attrs);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let json_keys: std::collections::BTreeSet<String> =
            v.as_object().expect("object").keys().cloned().collect();
        let attr_keys: std::collections::BTreeSet<String> =
            attrs.iter().map(|(k, _)| k.to_string()).collect();
        assert_eq!(json_keys, attr_keys, "JSONL keys must match attribute keys");
    }
}
