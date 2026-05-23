//! Lifecycle events stream — JSONL append to `<pillbox>/events.jsonl`.
//!
//! ## v0.7 spike scope
//!
//! Just enough surface to validate the orchestrator-consumer pattern
//! with bash + jq. Real PR 2 will:
//!   - Add webhook + OTel sinks behind an `Emitter` trait.
//!   - Cover the full lifecycle (`session.completed`, `session.failed`).
//!   - Lock the schemas formally with semver discipline.
//!
//! For the spike, only `session.started` and `session.dropped` are
//! emitted — that's the minimum needed to prove the JSONL transport
//! works end-to-end. `pillbox session events --follow` tails the file;
//! consumers (`examples/orchestrator/`) parse with jq.
//!
//! ## Field shape
//!
//! OTel-shaped from day one so PR 2's OTel exporter is a thin shim,
//! not a re-instrumentation:
//!
//! ```jsonc
//! {
//!   "event": "session.started",
//!   "session_id": "abc123def456",          // → OTel span_id
//!   "parent_session_id": "789...",         // → OTel parent_span_id (forks)
//!   "started_at": "2026-05-23T13:37:00Z",  // → OTel span.start_time
//!   "ended_at": null,                      // → OTel span.end_time (on done)
//!   "agent_id": "claude",
//!   "remote": "prod-cloud",
//!   "backend": "e2b"
//! }
//! ```
//!
//! Best-effort writes: a failed event emit logs a warning and
//! continues. The agent run is more important than the event log; the
//! orchestrator can tolerate a missed event (it'll see `dropped` later
//! anyway).

use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::pillbox::Pillbox;
use crate::session::{self, Session};

/// Filename under `<pillbox>/state_dir/`. Append-only JSONL.
pub(crate) const EVENTS_FILE: &str = "events.jsonl";

/// Polling interval for `--follow` mode. 200ms is fast enough for
/// human-paced session lifecycles and slow enough not to spin CPU.
/// Real PR 2 will use inotify / kqueue.
const FOLLOW_POLL_MS: u64 = 200;

#[derive(Debug, Clone, Copy)]
pub(crate) enum EventType {
    SessionStarted,
    SessionDropped,
    // PR 2: SessionCompleted, SessionFailed
}

impl EventType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EventType::SessionStarted => "session.started",
            EventType::SessionDropped => "session.dropped",
        }
    }
}

/// Emit one event for a session lifecycle transition. Never panics;
/// errors are logged to stderr so a broken events log doesn't kill the
/// run.
pub(crate) fn emit_session_event(pb: &Pillbox, ty: EventType, session: &Session) {
    if let Err(e) = emit_session_event_inner(pb, ty, session) {
        eprintln!(
            "pillbox: warning: failed to emit event {}: {e}",
            ty.as_str()
        );
    }
}

fn emit_session_event_inner(pb: &Pillbox, ty: EventType, session: &Session) -> Result<()> {
    let path = events_path(pb);
    // Ensure the state dir exists. Most callers run after a pillbox
    // command that's already touched it, but emission shouldn't depend
    // on a happens-before with init — a fresh isolated test environment
    // or a race against a deleted state dir shouldn't lose the event.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = build_event_json(ty, session);
    // 0600 because session ids + sandbox ids aren't secrets but the
    // events file lives alongside vault state — uniform perms.
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(file, "{body}")?;
    Ok(())
}

fn build_event_json(ty: EventType, session: &Session) -> String {
    let now = session::now_rfc3339();
    let ended_at = match ty {
        EventType::SessionDropped => serde_json::Value::String(now),
        _ => serde_json::Value::Null,
    };
    serde_json::json!({
        "event": ty.as_str(),
        "session_id": session.id,
        "started_at": session.started_at,
        "ended_at": ended_at,
        "agent_id": session.agent_id,
        "remote": session.remote,
        "backend": session.backend,
        "label": session.label,
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
    let mut last_size: u64 = 0;
    if path.exists() {
        let content =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        print!("{content}");
        last_size = content.len() as u64;
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
            let mut stdout = std::io::stdout();
            std::io::copy(&mut file, &mut stdout).with_context(|| "stream events to stdout")?;
            stdout.flush().ok();
            last_size = size;
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
    use crate::session::BACKEND_E2B;
    use crate::test_util::with_isolated_home;

    fn make_session() -> Session {
        Session {
            id: "abc123def456".to_string(),
            label: Some("test".to_string()),
            remote: "test-remote".to_string(),
            backend: BACKEND_E2B.to_string(),
            sandbox_id: "sb_test".to_string(),
            pty_pid: 0,
            agent_id: "claude".to_string(),
            started_at: "2026-05-23T13:37:00Z".to_string(),
            attached_pid: None,
        }
    }

    #[test]
    fn emit_appends_jsonl_line() {
        with_isolated_home("events-emit", || {
            let pb = pillbox::global();
            let s = make_session();
            emit_session_event(&pb, EventType::SessionStarted, &s);
            emit_session_event(&pb, EventType::SessionDropped, &s);
            let content = fs::read_to_string(events_path(&pb)).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            assert_eq!(lines.len(), 2);
            let started: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(started["event"], "session.started");
            assert_eq!(started["session_id"], "abc123def456");
            assert_eq!(started["ended_at"], serde_json::Value::Null);
            let dropped: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
            assert_eq!(dropped["event"], "session.dropped");
            assert!(
                !dropped["ended_at"].is_null(),
                "dropped should have ended_at"
            );
        });
    }

    #[test]
    fn build_event_includes_otel_shaped_fields() {
        let s = make_session();
        let raw = build_event_json(EventType::SessionStarted, &s);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // OTel-shaped names — verify the field set is what PR 2's
        // exporter will be able to consume without re-mapping.
        for field in &[
            "event",
            "session_id",
            "started_at",
            "ended_at",
            "agent_id",
            "remote",
            "backend",
            "label",
        ] {
            assert!(v.get(field).is_some(), "missing field: {field}");
        }
    }
}
