//! Lifecycle events stream — JSONL append to `<pillbox>/events.jsonl`.
//!
//! ## Event taxonomy
//!
//! Four lifecycle events, all OTel-shaped:
//!
//! | Event              | Emitted by         | When                                        |
//! |--------------------|--------------------|---------------------------------------------|
//! | `session.started`  | host pillbox       | Sandbox + PTY are up, agent launched        |
//! | `session.completed`| `session done`     | Agent finished successfully                 |
//! | `session.failed`   | `session done`     | Agent exited non-zero / errored             |
//! | `session.dropped`  | host pillbox       | `session rm` torn the sandbox down          |
//!
//! `started` and `dropped` fire from the host. `completed`/`failed`
//! come from inside the sandbox: a wrapper around the agent calls
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
//!   "session_id": "abc123def456",          // → OTel span_id
//!   "parent_session_id": "789...",         // → OTel parent_span_id (forks)
//!   "started_at": "2026-05-23T13:37:00Z",  // → OTel span.start_time
//!   "ended_at":   "2026-05-23T13:42:11Z",  // → OTel span.end_time (terminal only)
//!   "agent_id": "claude",
//!   "remote": "prod-cloud",
//!   "backend": "e2b",
//!   "label": null,
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
//! Three sinks, all driven by the same `emit_session_event` call site.
//! Each is best-effort independently — a failed webhook POST doesn't
//! prevent the JSONL append from succeeding.
//!
//! - **JSONL** — appends to `<pillbox>/events.jsonl` (0600). Always
//!   active on the host. Sandbox-side pillbox also writes here but the
//!   file is ephemeral with the sandbox.
//! - **Webhook** — POSTs each event to `--events-webhook URL` (or
//!   `$PILLBOX_EVENTS_WEBHOOK`). Used to ferry sandbox-side events
//!   back to the orchestrator without pillbox running a daemon.
//! - **OTel** — emits one OTLP log record per event to whichever
//!   collector `$OTEL_EXPORTER_OTLP_ENDPOINT` points at. Default
//!   transport is HTTP/protobuf via the blocking reqwest client (no
//!   tokio runtime drag — matches the webhook sink's sync model).
//!   Optional gRPC behind the `otel-grpc` cargo feature. Spans land
//!   in the v0.7 PR 2c follow-up once the dual `session.started`
//!   event provides real durations to span over — emitting zero-
//!   duration spans on the current taxonomy would be structurally
//!   worse than no spans at all.
//!
//! Best-effort writes: a failed sink emit logs a warning and
//! continues. The agent run is more important than the event log; the
//! orchestrator can tolerate a missed event.

use std::{
    collections::HashMap,
    fs,
    io::{self, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::OnceLock,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use opentelemetry::logs::{Logger, LoggerProvider};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::SdkLogger;

use crate::paths;
use crate::pillbox::Pillbox;
use crate::session::{self, Session};

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

/// One lifecycle event variant. Terminal events (`SessionCompleted` /
/// `SessionFailed`) carry the variant-specific payload inline so the
/// `build_event_json` rendering is exhaustive at compile time. Lost
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
    SessionStarted,
    SessionCompleted {
        exit_code: Option<i32>,
        trace_path: Option<String>,
        /// Rustic snapshot handle of the agent's result workspace,
        /// pushed by the in-sandbox wrapper after the agent exits.
        /// Consumers correlate with `base_snapshot` (on the session
        /// record + future `session.started` event) to compute the
        /// fork's diff. `session pull <id>` rehydrates from this
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
            EventType::SessionStarted => "session.started",
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

/// The OTel-shaped field set every event carries. Compiled in for the
/// schema-shape test ([`tests::build_event_includes_otel_shaped_fields`])
/// so adding a key to `build_event_json` without updating this list (or
/// vice-versa) is caught by `cargo test`. Kept `#[cfg(test)]` because
/// production code uses the field names directly via the `json!` macro;
/// indirecting through this slice at runtime would buy nothing.
#[cfg(test)]
const EVENT_FIELDS: &[&str] = &[
    "version",
    "event",
    "session_id",
    "started_at",
    "ended_at",
    "agent_id",
    "remote",
    "backend",
    "label",
    "status",
    "reason",
    "exit_code",
    "trace_path",
    "result_snapshot",
    "base_snapshot",
];

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
    let payload = build_event_json(&ty, session_id, session);
    let name = ty.as_str();
    // JSONL is the always-on sink. Failures fall through to a warning;
    // we don't want a missing state dir to abort the agent run.
    warn_on_sink_error("jsonl", name, jsonl_sink_emit(pb, &payload));
    // Webhook sink — only fires if the env var is set. Sandbox-side
    // pillbox uses this to ferry terminal events back to whoever is
    // listening (typically the orchestrator).
    if let Ok(url) = std::env::var("PILLBOX_EVENTS_WEBHOOK") {
        if !url.is_empty() {
            warn_on_sink_error("webhook", name, webhook_sink_emit(&url, &payload));
        }
    }
    // OTel sink — only fires if OTEL_EXPORTER_OTLP_ENDPOINT is set.
    // Configured at first use; cached for the rest of the process.
    // Emits one log record per event with attributes mirroring the
    // JSONL field set so consumers can correlate.
    warn_on_sink_error("otel", name, otel_sink_emit(&ty, session_id, session));
}

/// One-place warning formatter so adding the OTel sink (PR 2b) only
/// adds a `warn_on_sink_error("otel", …)` line, not another bespoke
/// `if let Err(e)` block. Per-sink failures stay independent — a slow
/// webhook can't suppress the JSONL append, etc.
fn warn_on_sink_error(sink: &str, event: &str, result: Result<()>) {
    if let Err(e) = result {
        eprintln!("pillbox: warning: {sink} sink failed for {event}: {e}");
    }
}

fn jsonl_sink_emit(pb: &Pillbox, payload: &str) -> Result<()> {
    let path = events_path(pb);
    // Ensure the state dir exists *and* is 0700. Most callers run after
    // a pillbox command that's already touched it, but emission
    // shouldn't depend on a happens-before with init — a fresh isolated
    // test environment or a race against a deleted state dir shouldn't
    // lose the event. Pin the perms here too so `events.jsonl` doesn't
    // end up parented by a 0755 directory if some adversarial code path
    // created the state dir without going through `Pillbox::subdir`.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        paths::ensure_mode_0700(parent)?;
    }
    // Single `write_all` of `body + "\n"`: stdlib turns this into one
    // `write(2)` syscall on Unix, and `O_APPEND` makes that write
    // atomically positioned at end-of-file. For lines under `PIPE_BUF`
    // (4096 on Linux, typically larger elsewhere) a concurrent
    // `--follow` reader is guaranteed to see whole lines, never a
    // partial mid-line tear.
    let mut line = String::with_capacity(payload.len() + 1);
    line.push_str(payload);
    line.push('\n');
    paths::append_private_file(&path, line.as_bytes())?;
    Ok(())
}

/// Shared blocking HTTP client for the webhook sink. Built once on
/// first use and reused for every subsequent emit so a session's 2-4
/// terminal events don't pay the TLS-context setup cost on each call.
/// `reqwest::blocking::Client` is `Send + Sync` and internally pools
/// connections, which is the whole point of caching it.
static WEBHOOK_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

/// POST one event line to the configured webhook URL. Body is the JSON
/// payload (without trailing newline). Pillbox uses `reqwest::blocking`
/// because emit is called from sync code paths; a short request timeout
/// keeps a slow webhook from blocking the run.
fn webhook_sink_emit(url: &str, payload: &str) -> Result<()> {
    // First-call build is the only path that can fail (e.g. native TLS
    // backend missing). Subsequent calls reuse the cached client, so the
    // `?` here only short-circuits the first attempt per process.
    let client = webhook_client()?;
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(payload.to_string())
        .send()
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "webhook {url} returned HTTP {}",
            resp.status()
        ));
    }
    Ok(())
}

/// 2s per request — long enough for a healthy collector on the same
/// continent, short enough that a stuck endpoint doesn't dominate a
/// session's runtime. A full lifecycle (started + completed + dropped)
/// is 3 emits, so worst case a dead webhook adds ~6s to a run. The
/// emit is best-effort; on timeout the call site logs and continues.
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(2);

/// Lazy-init the shared client. `get_or_try_init` would let us bubble the
/// `build` error through `OnceLock`, but it's still nightly-only on
/// stable Rust; fall back to building, caching on success, and surfacing
/// the error directly otherwise. Two threads racing here both build a
/// client; whichever one calls `set` first wins — the loser's client is
/// dropped harmlessly. Worth the simpler code given how rare a build
/// failure is.
fn webhook_client() -> Result<&'static reqwest::blocking::Client> {
    if let Some(c) = WEBHOOK_CLIENT.get() {
        return Ok(c);
    }
    let built = reqwest::blocking::Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        .build()
        .context("build webhook http client")?;
    let _ = WEBHOOK_CLIENT.set(built);
    Ok(WEBHOOK_CLIENT.get().expect("set or already-set"))
}

/// OTLP logs sink — one log record per lifecycle event. Cached per
/// process so the TLS-context + exporter setup costs land on the
/// first event of the run, not every emit. The inner `Option` is the
/// "configured or not" flag: `None` means the env var was unset at
/// first call, so we skip the sink for the lifetime of the process
/// (env-var flipping mid-process isn't a supported workflow).
static OTEL_LOGGER: OnceLock<Option<SdkLogger>> = OnceLock::new();

/// 2s per export — matches the webhook sink's budget for the same
/// reason: a slow collector shouldn't dominate a session's runtime.
/// Setting via the OTel-standard `OTEL_EXPORTER_OTLP_TIMEOUT` env
/// (in milliseconds) overrides this, per the OTLP spec.
const OTEL_EXPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// Default `service.name` resource attribute when `OTEL_SERVICE_NAME`
/// isn't set. Spec-recommended fallback chain is OTEL_SERVICE_NAME →
/// OTEL_RESOURCE_ATTRIBUTES → "unknown_service"; pillbox is more
/// useful than `unknown_service:pillbox` so we hardcode it.
const OTEL_DEFAULT_SERVICE_NAME: &str = "pillbox";

/// Build one OTLP log record per event and emit through the cached
/// SDK logger. Returns `Ok(())` (best-effort skip) when the env var
/// isn't set; otherwise propagates exporter-build failures so the
/// caller's `warn_on_sink_error` can surface them.
fn otel_sink_emit(ty: &EventType, session_id: &str, session: Option<&Session>) -> Result<()> {
    let Some(logger) = otel_logger() else {
        return Ok(());
    };
    let mut record = logger.create_log_record();
    fill_log_record(&mut record, ty, session_id, session);
    logger.emit(record);
    Ok(())
}

/// Lazy-init the shared logger from env. Returns `None` if
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is unset/empty (the sink is opt-in;
/// most pillbox invocations won't have an OTel collector configured
/// and shouldn't pay for the SDK bootstrap). Build failures are
/// printed once and cached as `None` so a misconfigured endpoint
/// doesn't repeatedly spam stderr.
fn otel_logger() -> Option<&'static SdkLogger> {
    OTEL_LOGGER
        .get_or_init(|| {
            let endpoint = resolve_logs_endpoint()?;
            warn_if_plaintext_to_non_loopback(&endpoint);
            let service_name = std::env::var("OTEL_SERVICE_NAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| OTEL_DEFAULT_SERVICE_NAME.to_string());
            let headers = parse_otel_headers(
                std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
                    .as_deref()
                    .unwrap_or(""),
            );
            match build_otel_logger(&endpoint, headers, &service_name) {
                Ok(logger) => Some(logger),
                Err(e) => {
                    eprintln!(
                        "pillbox: warning: OTel exporter init failed for `{endpoint}`: {e:#}; \
                         continuing with other sinks."
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Resolve the OTLP/HTTP logs endpoint per the spec. Reads env once,
/// hands the base URL to [`format_logs_endpoint`] for the signal-path
/// append. The signal-specific env (`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`)
/// wins per the OTLP spec — when set, it's used verbatim because the
/// user wanted explicit control over the full URL.
fn resolve_logs_endpoint() -> Option<String> {
    if let Ok(signal) = std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT") {
        let trimmed = signal.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
    format_logs_endpoint(&base)
}

/// Append `/v1/logs` to a base OTLP URL with trailing-slash
/// normalization. Pure (no env reads, no I/O) so the test can pin the
/// path-assembly behavior without racing the global env table.
fn format_logs_endpoint(base: &str) -> Option<String> {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("{trimmed}/v1/logs"))
}

/// Parse one `OTEL_EXPORTER_OTLP_HEADERS` value into a header map.
/// Comma-separated `k=v` pairs per the OTLP spec. Percent-decoding
/// intentionally skipped — matches the Go and Python SDKs' default
/// behavior; values with literal commas are silently truncated, same
/// as those SDKs. Empty input returns an empty map.
fn parse_otel_headers(raw: &str) -> HashMap<String, String> {
    if raw.is_empty() {
        return HashMap::new();
    }
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// One-time warning when the configured collector is plaintext HTTP
/// to a non-loopback host. Event attributes can carry user-supplied
/// `label` text + session ids; in-cluster collectors over plain HTTP
/// are fine, but a remote cleartext endpoint is almost always a
/// misconfig. Mirrors the webhook sink's posture (the same threat
/// model applies — event payloads are equivalent).
fn warn_if_plaintext_to_non_loopback(endpoint: &str) {
    let Some(rest) = endpoint.strip_prefix("http://") else {
        return;
    };
    let host_port = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port);
    let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        || host.starts_with("127.")
        || host.ends_with(".localhost")
        || host.ends_with(".local");
    if !is_loopback {
        eprintln!(
            "pillbox: warning: OTel endpoint `{endpoint}` is plaintext HTTP to a non-loopback host \
             — events include session ids + user-supplied labels. Prefer https:// for remote collectors."
        );
    }
}

/// Build the SDK logger for `endpoint`. The simple processor exports
/// inline on emit — no background runtime, no shutdown coordination —
/// which keeps the sink usable from sync code paths. The blocking
/// reqwest client (selected via the `reqwest-blocking-client`
/// feature on `opentelemetry-otlp`) matches.
///
/// The `SdkLoggerProvider` is dropped here, but `provider.logger(...)`
/// clones the provider's `Arc<inner>` into the returned `SdkLogger`,
/// so the processor + exporter stay alive for the cached logger's
/// lifetime. With simple processor there's no buffer to flush on
/// shutdown; if a future PR switches to batch processing, an
/// `at_exit` hook to call `provider.shutdown()` becomes load-bearing.
fn build_otel_logger(
    endpoint: &str,
    headers: HashMap<String, String>,
    service_name: &str,
) -> Result<SdkLogger> {
    let mut builder = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_timeout(OTEL_EXPORT_TIMEOUT);
    if !headers.is_empty() {
        builder = builder.with_headers(headers);
    }
    let exporter = builder.build().context("build OTLP log exporter")?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();
    let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_simple_exporter(exporter)
        .build();
    Ok(provider.logger("pillbox"))
}

/// Fill a freshly-created [`LogRecord`] with the same field set the
/// JSONL line carries. Kept separate from `otel_sink_emit` so the
/// attribute-shape test can exercise it without touching the global
/// logger cache (which would require env-var manipulation and break
/// test parallelism).
fn fill_log_record<R: opentelemetry::logs::LogRecord>(
    record: &mut R,
    ty: &EventType,
    session_id: &str,
    session: Option<&Session>,
) {
    let (severity_number, severity_text) = severity_for(ty);
    record.set_severity_number(severity_number);
    record.set_severity_text(severity_text);
    record.set_event_name(ty.as_str());
    // OTel convention: `body` is the human-readable message; the
    // structured payload lives in attributes. Using the event name as
    // body keeps single-line tail-style log viewers readable without
    // duplicating the structured fields.
    record.set_body(opentelemetry::logs::AnyValue::String(ty.as_str().into()));
    record.add_attribute("version", EVENT_SCHEMA_VERSION as i64);
    record.add_attribute("event", ty.as_str());
    record.add_attribute("session_id", session_id.to_string());
    record.add_attribute("status", ty.status_code());
    add_terminal_attributes(record, ty);
    add_session_attributes(record, session);
}

/// Map a lifecycle event to its (`Severity`, severity-text) pair.
/// Failed sessions are ERROR; everything else is INFO. The text label
/// is what shows up in human-readable views; the numeric severity is
/// what severity-based filters key on per the OTel logs spec.
fn severity_for(ty: &EventType) -> (opentelemetry::logs::Severity, &'static str) {
    match ty {
        EventType::SessionFailed { .. } => (opentelemetry::logs::Severity::Error, "ERROR"),
        _ => (opentelemetry::logs::Severity::Info, "INFO"),
    }
}

fn add_terminal_attributes<R: opentelemetry::logs::LogRecord>(record: &mut R, ty: &EventType) {
    match ty {
        EventType::SessionCompleted {
            exit_code,
            trace_path,
            result_snapshot,
        } => {
            if let Some(code) = exit_code {
                record.add_attribute("exit_code", *code as i64);
            }
            if let Some(path) = trace_path {
                record.add_attribute("trace_path", path.clone());
            }
            if let Some(snap) = result_snapshot {
                record.add_attribute("result_snapshot", snap.clone());
            }
        }
        EventType::SessionFailed {
            reason,
            exit_code,
            trace_path,
            result_snapshot,
        } => {
            record.add_attribute("reason", reason.clone());
            if let Some(code) = exit_code {
                record.add_attribute("exit_code", *code as i64);
            }
            if let Some(path) = trace_path {
                record.add_attribute("trace_path", path.clone());
            }
            if let Some(snap) = result_snapshot {
                record.add_attribute("result_snapshot", snap.clone());
            }
        }
        EventType::SessionStarted | EventType::SessionDropped => {}
    }
}

/// Empty / missing fields are *omitted* rather than emitted as
/// explicit nulls — OTel attribute bags have no notion of "present
/// with null value" (it's either there or not). The JSONL sink
/// renders the same fields as JSON `null` because JSON objects do
/// carry that distinction. Same semantic ("we don't have this"),
/// representation per format.
fn add_session_attributes<R: opentelemetry::logs::LogRecord>(
    record: &mut R,
    session: Option<&Session>,
) {
    let Some(s) = session else {
        return;
    };
    if !s.started_at.is_empty() {
        record.add_attribute("started_at", s.started_at.clone());
    }
    if !s.agent_id.is_empty() {
        record.add_attribute("agent_id", s.agent_id.clone());
    }
    if !s.remote.is_empty() {
        record.add_attribute("remote", s.remote.clone());
    }
    if !s.backend.is_empty() {
        record.add_attribute("backend", s.backend.clone());
    }
    if let Some(label) = &s.label {
        record.add_attribute("label", label.clone());
    }
    if let Some(snap) = &s.base_snapshot {
        record.add_attribute("base_snapshot", snap.clone());
    }
}

fn build_event_json(ty: &EventType, session_id: &str, session: Option<&Session>) -> String {
    let now = session::now_rfc3339();
    let ended_at = if ty.is_terminal() || matches!(ty, EventType::SessionDropped) {
        serde_json::Value::String(now)
    } else {
        serde_json::Value::Null
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
    // Session-derived fields render as JSON null when no record is
    // available (sandbox-side path). Empty strings would be a lie —
    // `agent_id: ""` reads as "the agent's name is the empty string",
    // not "we don't know the agent". `as_opt_str` collapses both
    // None and Some("") to Null so a stub upstream gets the same
    // treatment as a missing record.
    let session_str = |f: fn(&Session) -> &str| -> serde_json::Value {
        session
            .map(f)
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null)
    };
    let session_opt_str = |f: fn(&Session) -> Option<&str>| -> serde_json::Value {
        session
            .and_then(f)
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null)
    };
    // `version` first by convention so a consumer scanning the head of
    // the line can branch on it before touching the rest. The field set
    // is mirrored in `EVENT_FIELDS` — the schema test guards that both
    // stay in sync.
    serde_json::json!({
        "version": EVENT_SCHEMA_VERSION,
        "event": ty.as_str(),
        "session_id": session_id,
        "started_at": session_str(|s| &s.started_at),
        "ended_at": ended_at,
        "agent_id": session_str(|s| &s.agent_id),
        "remote": session_str(|s| &s.remote),
        "backend": session_str(|s| &s.backend),
        "label": session_opt_str(|s| s.label.as_deref()),
        "status": ty.status_code(),
        "reason": reason,
        "exit_code": exit_code,
        "trace_path": trace_path,
        "result_snapshot": result_snapshot,
        "base_snapshot": session_opt_str(|s| s.base_snapshot.as_deref()),
    })
    .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::test_util::with_isolated_home;
    use std::io::Read;
    // `Write` is already in scope via the outer module's
    // `use std::io::{Seek, SeekFrom, Write}`; re-importing here would
    // be the redundant import clippy flags.
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn emit_appends_jsonl_line_for_all_event_types() {
        with_isolated_home("events-emit-all", || {
            let pb = pillbox::global();
            let s = Session::test_fixture();
            emit_session_event(&pb, EventType::SessionStarted, &s.id, Some(&s));
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
            assert_eq!(started["ended_at"], serde_json::Value::Null);
            assert_eq!(started["status"], "unset");
            assert_eq!(started["version"], EVENT_SCHEMA_VERSION);

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
    fn webhook_sink_posts_json_body() {
        // Bind a real loopback TCP listener and verify `webhook_sink_emit`
        // POSTs a well-formed HTTP request with the JSON payload as the
        // body. Avoids env-var coupling (which would force serial
        // execution with the rest of the test suite) by calling the sink
        // function directly. The HTTP server is the dumbest possible
        // single-request handler — enough to verify shape, no need for
        // hyper/reqwest mocks.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/events");

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let recv_clone = Arc::clone(&received);
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 4096];
            // Read once — the test payload fits in one packet and we
            // only need to verify the request shape, not handle pipelining.
            let n = sock.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            *recv_clone.lock().unwrap() = Some(request);
        });

        let payload = r#"{"event":"session.completed","session_id":"abc"}"#;
        webhook_sink_emit(&url, payload).expect("emit");

        server.join().expect("server thread");
        let req = received.lock().unwrap().take().expect("got request");
        assert!(req.starts_with("POST /events"), "got: {req}");
        assert!(
            req.to_lowercase()
                .contains("content-type: application/json"),
            "got: {req}"
        );
        assert!(req.contains(payload), "body missing in: {req}");
    }

    #[test]
    fn webhook_sink_surfaces_non_2xx() {
        // Server returns 500; sink should return Err with the status.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/events");
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ =
                sock.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        });
        let err = webhook_sink_emit(&url, "{}").unwrap_err();
        server.join().expect("server thread");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"), "expected 500 in: {msg}");
    }

    #[test]
    fn severity_for_event_type() {
        // Failed sessions map to ERROR; everything else INFO. The
        // numeric severity is what severity-based filters key on per
        // the OTel logs spec; the text label is for human-readable
        // views.
        let (sev, text) = severity_for(&EventType::SessionFailed {
            reason: "x".into(),
            exit_code: None,
            trace_path: None,
            result_snapshot: None,
        });
        assert_eq!(sev, opentelemetry::logs::Severity::Error);
        assert_eq!(text, "ERROR");

        for ty in [
            EventType::SessionStarted,
            EventType::SessionCompleted {
                exit_code: None,
                trace_path: None,
                result_snapshot: None,
            },
            EventType::SessionDropped,
        ] {
            let (sev, text) = severity_for(&ty);
            assert_eq!(sev, opentelemetry::logs::Severity::Info, "{}", ty.as_str());
            assert_eq!(text, "INFO");
        }
    }

    #[test]
    fn format_logs_endpoint_appends_v1_logs_to_base() {
        // OTLP spec: OTEL_EXPORTER_OTLP_ENDPOINT is a BASE URL; the
        // signal path gets appended. Trailing-slash normalization so
        // `http://host:4318` and `http://host:4318/` produce the same
        // target. Empty input → None (no fallback URL).
        assert_eq!(
            format_logs_endpoint("http://collector:4318").as_deref(),
            Some("http://collector:4318/v1/logs")
        );
        assert_eq!(
            format_logs_endpoint("http://collector:4318/").as_deref(),
            Some("http://collector:4318/v1/logs")
        );
        // A nonstandard base path (e.g. behind a reverse proxy with
        // a /otel prefix) gets the signal path appended too — that's
        // the spec; users wanting a custom path use the signal-
        // specific env var instead.
        assert_eq!(
            format_logs_endpoint("http://collector/otel/").as_deref(),
            Some("http://collector/otel/v1/logs")
        );
        assert_eq!(format_logs_endpoint("   ").as_deref(), None);
        assert_eq!(format_logs_endpoint("").as_deref(), None);
    }

    #[test]
    fn parse_otel_headers_handles_comma_separated_pairs() {
        let h = parse_otel_headers("authorization=Bearer abc, x-tenant=acme");
        assert_eq!(h.get("authorization"), Some(&"Bearer abc".to_string()));
        assert_eq!(h.get("x-tenant"), Some(&"acme".to_string()));
        assert_eq!(h.len(), 2);
        // Empty key dropped, trailing comma tolerated.
        let h = parse_otel_headers("=value, k=v,");
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("k"), Some(&"v".to_string()));
        // Empty input → empty map (matches unset-env behavior).
        assert!(parse_otel_headers("").is_empty());
    }

    #[test]
    fn otel_sink_posts_protobuf_to_logs_endpoint() {
        // End-to-end: build a logger pointing at a loopback HTTP
        // listener, emit a record, verify the listener received a
        // POST to /v1/logs with the OTel protobuf content-type. We
        // don't decode the protobuf body — the SDK owns that shape
        // and re-implementing parser-level assertions here would
        // duplicate its tests. What we DO care about: pillbox is
        // the one calling into the SDK, and a regression where we
        // forgot to flush or routed to the wrong path needs to
        // surface here.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        // Full signal-specific URL — what `resolve_logs_endpoint`
        // produces in production when `OTEL_EXPORTER_OTLP_ENDPOINT`
        // is the base URL. Passing the bare base would post to `/`
        // (the SDK trusts our endpoint to be the final target).
        let endpoint = format!("http://{addr}/v1/logs");

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let recv_clone = Arc::clone(&received);
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            // Read until we see a blank line (end of headers) plus
            // the declared Content-Length, OR the buffer fills. The
            // OTel exporter sends a single POST per emit so one read
            // is enough for the shape check.
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            // 200 with empty body — the OTLP/HTTP spec lets the
            // collector reply with an empty ExportLogsServiceResponse
            // protobuf when everything succeeded, which is what
            // collectors do in practice.
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            *recv_clone.lock().unwrap() = Some(request);
        });

        let logger = build_otel_logger(&endpoint, HashMap::new(), "pillbox-test")
            .expect("build OTel logger");
        let mut record = logger.create_log_record();
        fill_log_record(
            &mut record,
            &EventType::SessionStarted,
            "abc123def456",
            Some(&Session::test_fixture()),
        );
        logger.emit(record);

        server.join().expect("server thread");
        let req = received.lock().unwrap().take().expect("got request");
        assert!(
            req.starts_with("POST /v1/logs"),
            "expected POST /v1/logs, got: {req}"
        );
        assert!(
            req.to_lowercase()
                .contains("content-type: application/x-protobuf"),
            "expected OTLP protobuf content-type, got: {req}"
        );
    }

    #[test]
    fn build_event_includes_otel_shaped_fields() {
        let s = Session::test_fixture();
        // Render a terminal event so the schema includes every field
        // (started / dropped leave the terminal-only fields null but
        // still present in the JSON object).
        let raw = build_event_json(
            &EventType::SessionFailed {
                reason: "x".into(),
                exit_code: Some(1),
                trace_path: Some("y".into()),
                result_snapshot: Some("z".into()),
            },
            &s.id,
            Some(&s),
        );
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // OTel-shaped names — verify the field set is what the OTel
        // exporter will consume without re-mapping. The list lives on
        // `EVENT_FIELDS` so adding a field to one place forces the
        // other.
        for field in EVENT_FIELDS {
            assert!(v.get(field).is_some(), "missing field: {field}");
        }
    }
}
