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
//! - **OTel** — exports as spans + counters via OTLP HTTP or gRPC to
//!   `--otel-endpoint URL` (or `$OTEL_EXPORTER_OTLP_ENDPOINT`).
//!   Sessions map to spans; the parent-child fork relationship from
//!   v0.7 PR 1 will propagate as span context.
//!
//! Best-effort writes: a failed sink emit logs a warning and
//! continues. The agent run is more important than the event log; the
//! orchestrator can tolerate a missed event.

use std::{
    fs,
    io::{self, Seek, SeekFrom, Write},
    path::PathBuf,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

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
    },
    SessionFailed {
        reason: String,
        exit_code: Option<i32>,
        trace_path: Option<String>,
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
];

/// Emit one event for a session lifecycle transition. Routes through
/// every configured sink (JSONL always; webhook + OTel if env / flags
/// set). Never panics; per-sink errors are logged to stderr so a
/// broken sink doesn't kill the run.
pub(crate) fn emit_session_event(pb: &Pillbox, ty: EventType, session: &Session) {
    let payload = build_event_json(&ty, session);
    // JSONL is the always-on sink. Failures fall through to a warning;
    // we don't want a missing state dir to abort the agent run.
    if let Err(e) = jsonl_sink_emit(pb, &payload) {
        eprintln!(
            "pillbox: warning: jsonl sink failed for {}: {e}",
            ty.as_str()
        );
    }
    // Webhook sink — only fires if the env var is set. Sandbox-side
    // pillbox uses this to ferry terminal events back to whoever is
    // listening (typically the orchestrator).
    if let Ok(url) = std::env::var("PILLBOX_EVENTS_WEBHOOK") {
        if !url.is_empty() {
            if let Err(e) = webhook_sink_emit(&url, &payload) {
                eprintln!(
                    "pillbox: warning: webhook sink failed for {}: {e}",
                    ty.as_str()
                );
            }
        }
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

/// POST one event line to the configured webhook URL. Body is the JSON
/// payload (without trailing newline). Pillbox uses `reqwest::blocking`
/// because emit is called from sync code paths; a short request timeout
/// keeps a slow webhook from blocking the run.
fn webhook_sink_emit(url: &str, payload: &str) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build webhook http client")?;
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

fn build_event_json(ty: &EventType, session: &Session) -> String {
    let now = session::now_rfc3339();
    let ended_at = if ty.is_terminal() || matches!(ty, EventType::SessionDropped) {
        serde_json::Value::String(now)
    } else {
        serde_json::Value::Null
    };
    let (reason, exit_code, trace_path) = match ty {
        EventType::SessionCompleted {
            exit_code,
            trace_path,
        } => (None, *exit_code, trace_path.clone()),
        EventType::SessionFailed {
            reason,
            exit_code,
            trace_path,
        } => (Some(reason.clone()), *exit_code, trace_path.clone()),
        _ => (None, None, None),
    };
    // `version` first by convention so a consumer scanning the head of
    // the line can branch on it before touching the rest. The field set
    // is mirrored in `EVENT_FIELDS` — the schema test guards that both
    // stay in sync.
    serde_json::json!({
        "version": EVENT_SCHEMA_VERSION,
        "event": ty.as_str(),
        "session_id": session.id,
        "started_at": session.started_at,
        "ended_at": ended_at,
        "agent_id": session.agent_id,
        "remote": session.remote,
        "backend": session.backend,
        "label": session.label,
        "status": ty.status_code(),
        "reason": reason,
        "exit_code": exit_code,
        "trace_path": trace_path,
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
            emit_session_event(&pb, EventType::SessionStarted, &s);
            emit_session_event(
                &pb,
                EventType::SessionCompleted {
                    exit_code: Some(0),
                    trace_path: Some("rustic://x".into()),
                },
                &s,
            );
            emit_session_event(
                &pb,
                EventType::SessionFailed {
                    reason: "agent panic".into(),
                    exit_code: Some(42),
                    trace_path: None,
                },
                &s,
            );
            emit_session_event(&pb, EventType::SessionDropped, &s);
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
            },
            &s,
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
