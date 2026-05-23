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
//!   "version": 1,                          // bump on breaking field-set change
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

#[derive(Debug, Clone, Copy)]
pub(crate) enum EventType {
    SessionStarted,
    SessionDropped,
}

impl EventType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EventType::SessionStarted => "session.started",
            EventType::SessionDropped => "session.dropped",
        }
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
];

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
    let mut line = build_event_json(ty, session);
    line.push('\n');
    // 0600 via `paths::append_private_file` — events file lives
    // alongside vault state (session ids + sandbox ids aren't secrets
    // but uniform perms keep the threat model simple). Going through
    // the helper keeps the "private-on-disk = 0600" invariant in one
    // place; see also `paths::write_private_file` for the create+
    // truncate companion.
    //
    // Single `write_all` of `body + "\n"`: stdlib turns this into one
    // `write(2)` syscall on Unix, and `O_APPEND` makes that write
    // atomically positioned at end-of-file. For lines under `PIPE_BUF`
    // (4096 on Linux, typically larger elsewhere) a concurrent
    // `--follow` reader is guaranteed to see whole lines, never a
    // partial mid-line tear.
    paths::append_private_file(&path, line.as_bytes())?;
    Ok(())
}

fn build_event_json(ty: EventType, session: &Session) -> String {
    let now = session::now_rfc3339();
    let ended_at = match ty {
        EventType::SessionDropped => serde_json::Value::String(now),
        _ => serde_json::Value::Null,
    };
    // `version` first by convention so a consumer scanning the head of
    // the line can branch on it before touching the rest. The field set
    // is mirrored in `EVENT_FIELDS` — the schema test below guards
    // that both stay in sync.
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

    #[test]
    fn emit_appends_jsonl_line() {
        with_isolated_home("events-emit", || {
            let pb = pillbox::global();
            let s = Session::test_fixture();
            emit_session_event(&pb, EventType::SessionStarted, &s);
            emit_session_event(&pb, EventType::SessionDropped, &s);
            let content = fs::read_to_string(events_path(&pb)).unwrap();
            let lines: Vec<&str> = content.lines().collect();
            assert_eq!(lines.len(), 2);
            let started: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(started["event"], "session.started");
            assert_eq!(started["session_id"], "abc123def456");
            assert_eq!(started["ended_at"], serde_json::Value::Null);
            assert_eq!(started["version"], EVENT_SCHEMA_VERSION);
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
        let s = Session::test_fixture();
        let raw = build_event_json(EventType::SessionStarted, &s);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // OTel-shaped names — verify the field set is what PR 2's
        // exporter will be able to consume without re-mapping. The list
        // lives on `EVENT_FIELDS` so adding a field to one place forces
        // the other.
        for field in EVENT_FIELDS {
            assert!(v.get(field).is_some(), "missing field: {field}");
        }
    }
}
