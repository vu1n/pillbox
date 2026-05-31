//! The per-session durable event log — the vNext §0 keystone spine.
//!
//! One append-only, per-session log of [`Event`]s (the `contract.rs` envelope)
//! at `<pillbox>/sessions/<id>/log.jsonl`. This is the single foundation every
//! consumer reads — a chat bridge, the DSPy/RLM loop, an orchestrator, a
//! terminal renderer — and every surface (CLI / WS / gRPC) is a thin adapter
//! over it. Full design: [docs/session-event-log.md](../../docs/session-event-log.md).
//!
//! ## What this slice is
//!
//! The load-bearing core that's expensive to retrofit:
//!   - **per-session sequencer** — [`SessionLog::append`] assigns the monotonic
//!     `seq` (the log is the seq authority, not the producer). This is the
//!     co-located single-writer case the spec says to ship first; the
//!     remote-disconnect / multi-writer reconciliation is deferred and gated.
//!   - **replay** — [`SessionLog::read_from`] returns the tail from a `seq`, so
//!     a late-joining subscriber catches up deterministically.
//!
//! ## What it deliberately is NOT (yet) — all additive, see the spec
//!
//!   - **`Payload::Unknown`** forward-compat: [`read_from`](SessionLog::read_from)
//!     parses each line as a known [`Payload`]; an unknown future payload errors
//!     rather than degrading. Lands with the first real producer + foreign-trace
//!     ingest. (The seq scan on open is already forward-compatible — it parses
//!     only `{seq}` — so a newer payload never breaks *append*.)
//!   - **live subscribe-follow**: a thin loop over `read_from` + an FS notify,
//!     mirroring [`crate::events::transcripts::Tailer`]. Next slice.
//!   - **producers**: wiring the harness parser (`agents/harness`, which already
//!     produces [`Payload`]s) and the lifecycle stream into the log.
//!   - **`actor` / `class` envelope fields**: land with the gateway (authenticated
//!     actor) and the pooling gate (content-vs-signal) that enforce them.
//!   - **blob store / `pty_snapshot` / `raw_body`** and the `head` fast-resume
//!     file: optimizations that arrive with their consumers.
//!
//! Pure functional core — no docker/agent/network. Fully unit-tested below.

// The log's public surface lands ahead of its first producer (the harness →
// log wiring is the next slice), mirroring contract.rs's contract-first stance.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::contract::Event;
use crate::paths::append_private_file;
use crate::pillbox::Pillbox;

/// The append-only spine file inside a session's directory.
const LOG_FILE: &str = "log.jsonl";

/// Append-only durable event log for one session, backed by a local JSONL
/// file. Holds the seq authority for the session: each [`append`](Self::append)
/// stamps the next per-session `seq`. Single-writer — co-located producers
/// hold one `SessionLog` and submit through it (the spec's "ship first" case).
pub(crate) struct SessionLog {
    /// `<pillbox>/sessions/<id>/` — the per-session directory (0700). Shares
    /// the `sessions/` parent with the `<id>.toml` record (file vs. dir, no
    /// collision).
    dir: PathBuf,
    /// Highest `seq` durably in the log. Derived from the log on open (the log
    /// is authoritative, so a crash can never hand out a duplicate seq), then
    /// advanced in memory per append.
    last_seq: u64,
}

impl SessionLog {
    /// Open (creating if absent) the log for `session_id` under `pb`'s state
    /// dir. Recovers `last_seq` from the existing log so appends continue the
    /// sequence across process restarts and reattaches.
    pub(crate) fn open(pb: &Pillbox, session_id: &str) -> Result<Self> {
        let dir = crate::session::session_dir(pb, session_id)?;
        let last_seq = recover_last_seq(&dir.join(LOG_FILE))?;
        Ok(Self { dir, last_seq })
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_FILE)
    }

    /// The highest durable `seq` assigned so far. A subscriber that has
    /// consumed up to this point is fully caught up.
    pub(crate) fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Append `events` durably, assigning each the next per-session `seq`
    /// (overwriting any `seq` the producer set — the log is the authority).
    /// Returns the last `seq` assigned (or the unchanged `last_seq` for an
    /// empty batch). One file open + write per call, so a batch is one
    /// syscall's worth of work and lands atomically per line.
    pub(crate) fn append(&mut self, events: &[Event]) -> Result<u64> {
        if events.is_empty() {
            return Ok(self.last_seq);
        }
        let mut seq = self.last_seq;
        let mut buf = String::new();
        for ev in events {
            seq += 1;
            let line = Event { seq, ..ev.clone() };
            buf.push_str(&serde_json::to_string(&line).context("serialize log event")?);
            buf.push('\n');
        }
        append_private_file(&self.log_path(), buf.as_bytes())?;
        self.last_seq = seq;
        Ok(seq)
    }

    /// Replay every durable event with `seq >= from`. `read_from(0)` is the
    /// full log; `read_from(last_seq + 1)` is "nothing new yet". Returns an
    /// empty vec when the log doesn't exist (a session with no events).
    pub(crate) fn read_from(&self, from: u64) -> Result<Vec<Event>> {
        let path = self.log_path();
        fold_lines(&path, Vec::new(), |mut out, line| {
            let ev: Event = serde_json::from_str(line)
                .with_context(|| format!("parse log line in {}", path.display()))?;
            if ev.seq >= from {
                out.push(ev);
            }
            Ok(out)
        })
    }

    /// Stream events with `seq >= from` to `sink`, then keep tailing the log,
    /// invoking `sink` for each newly-appended event as it lands. Returns when
    /// `stop` is set or `sink` returns `false` ("I've seen enough"). This is
    /// the live read side of the spine — what a WS/gRPC gateway adapter sits on
    /// to relay a running session to a remote consumer.
    ///
    /// Wakes on an `notify` FS event for the session dir, so an append — by
    /// this process or another (the file is the cross-process bus) — is
    /// relayed promptly, with a poll as a fallback for coalesced/missed events
    /// (macOS). Mirrors the transcript [`Tailer`](crate::events::transcripts)'s
    /// follow loop; the file stays the single bus (no in-process push channel),
    /// so every reader — local WS, a future gRPC/SSE adapter — improves
    /// uniformly. Each wake re-reads the tail via [`read_from`](Self::read_from)
    /// — fine at per-session scale; the byte-offset/`head` incremental read is
    /// the deferred optimization (see the module docs).
    pub(crate) fn subscribe(
        &self,
        from: u64,
        stop: &AtomicBool,
        mut sink: impl FnMut(&Event) -> bool,
    ) -> Result<()> {
        use notify::{RecursiveMode, Watcher};

        // Watch the session dir (the log file may not exist yet) before the
        // first drain, so an append between drain and wait isn't missed. The
        // dir, not the parent: `log.jsonl` is append-only (never rotated/
        // renamed), so a direct watch catches every append — unlike the
        // transcript `Tailer`, which watches the parent because a harness can
        // atomically rename its transcript.
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .context("build subscribe watcher")?;
        watcher
            .watch(&self.dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("watch {}", self.dir.display()))?;

        let mut next = from;
        loop {
            for ev in self.read_from(next)? {
                next = ev.seq + 1;
                if !sink(&ev) {
                    return Ok(());
                }
            }
            // Drain before checking `stop` so events appended right before the
            // stop signal still reach the subscriber.
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            match rx.recv_timeout(SUBSCRIBE_POLL) {
                // FS event (Ok/Err) or the poll fallback: loop and drain.
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Watcher dropped/channel closed — nothing more will wake us.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
}

/// Fallback poll interval for [`SessionLog::subscribe`] — `notify` does the
/// real low-latency relaying; this just bounds how long a missed FS event (or
/// a `stop` set while we're parked on `recv`) can stall, so it can be lazy.
const SUBSCRIBE_POLL: Duration = Duration::from_millis(500);

/// Fold over the non-empty JSONL lines of `path`, returning `init` unchanged
/// when the file doesn't exist (an empty / never-written log). Shared by replay
/// and seq recovery, which differ only in what they parse from each line — the
/// file read, `NotFound` tolerance, and empty-line skip live here once.
fn fold_lines<T>(path: &Path, init: T, mut f: impl FnMut(T, &str) -> Result<T>) -> Result<T> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(init),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut acc = init;
    for line in contents.lines() {
        if !line.trim().is_empty() {
            acc = f(acc, line)?;
        }
    }
    Ok(acc)
}

/// Recover the sequencer position by scanning the log for its maximum `seq`.
/// Parses only the `seq` field (not the payload) so it stays correct even once
/// newer payload variants exist that this binary can't fully decode — append
/// never hands out a duplicate seq. A line missing `seq` is corruption we
/// surface rather than silently extend past.
fn recover_last_seq(log_path: &Path) -> Result<u64> {
    #[derive(Deserialize)]
    struct SeqOnly {
        seq: u64,
    }
    fold_lines(log_path, 0, |max, line| {
        let s: SeqOnly = serde_json::from_str(line)
            .with_context(|| format!("read seq from {}", log_path.display()))?;
        Ok(max.max(s.seq))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Custom, Payload, ToolCall, ToolStatus};
    use crate::test_util::with_isolated_home;

    fn tool_call(name: &str) -> Payload {
        Payload::ToolCall(ToolCall {
            tool_call_id: format!("tc-{name}"),
            name: name.into(),
            status: ToolStatus::Running,
            input: None,
            output: String::new(),
            title: String::new(),
        })
    }

    #[test]
    fn append_assigns_monotonic_per_session_seq() {
        with_isolated_home("log-append-seq", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-1").unwrap();
            // Producer-set seq is ignored; the log assigns from 1.
            let last = log
                .append(&[
                    Event::session("sess-1", tool_call("a")),
                    Event::session("sess-1", tool_call("b")),
                ])
                .unwrap();
            assert_eq!(last, 2);
            assert_eq!(log.last_seq(), 2);
            let next = log
                .append(&[Event::session("sess-1", tool_call("c"))])
                .unwrap();
            assert_eq!(next, 3, "seq continues across append calls");

            let all = log.read_from(0).unwrap();
            let seqs: Vec<u64> = all.iter().map(|e| e.seq).collect();
            assert_eq!(seqs, vec![1, 2, 3], "monotonic, gap-free, in order");
        });
    }

    #[test]
    fn seq_resumes_across_reopen() {
        with_isolated_home("log-resume", || {
            let pb = crate::pillbox::global();
            {
                let mut log = SessionLog::open(&pb, "sess-2").unwrap();
                log.append(&[Event::session("sess-2", tool_call("a"))])
                    .unwrap();
                log.append(&[Event::session("sess-2", tool_call("b"))])
                    .unwrap();
            }
            // A fresh handle (process restart / reattach) must continue the
            // sequence, never re-issue seq 1.
            let mut reopened = SessionLog::open(&pb, "sess-2").unwrap();
            assert_eq!(reopened.last_seq(), 2, "recovered from the log on open");
            let next = reopened
                .append(&[Event::session("sess-2", tool_call("c"))])
                .unwrap();
            assert_eq!(next, 3);
        });
    }

    #[test]
    fn read_from_returns_only_the_tail() {
        with_isolated_home("log-read-from", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-3").unwrap();
            log.append(&[
                Event::session("sess-3", tool_call("a")),
                Event::session("sess-3", tool_call("b")),
                Event::session("sess-3", tool_call("c")),
            ])
            .unwrap();
            // A subscriber caught up through seq 1 asks for the rest.
            let tail = log.read_from(2).unwrap();
            assert_eq!(tail.len(), 2);
            assert_eq!(tail[0].seq, 2);
            assert_eq!(tail[1].seq, 3);
            // Caught all the way up: nothing new.
            assert!(log.read_from(4).unwrap().is_empty());
        });
    }

    #[test]
    fn payload_round_trips_through_the_file() {
        with_isolated_home("log-roundtrip", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-4").unwrap();
            log.append(&[Event::session(
                "sess-4",
                Payload::Custom(Custom {
                    name: "marker".into(),
                    payload: Some(serde_json::json!({"k": "v"})),
                }),
            )])
            .unwrap();
            let back = log.read_from(0).unwrap();
            assert_eq!(back.len(), 1);
            assert_eq!(back[0].session_id, "sess-4");
            let Payload::Custom(c) = &back[0].payload else {
                panic!("wrong variant: {:?}", back[0].payload);
            };
            assert_eq!(c.name, "marker");
            assert_eq!(c.payload.as_ref().unwrap()["k"], "v");
        });
    }

    #[test]
    fn read_from_missing_log_is_empty_not_error() {
        with_isolated_home("log-missing", || {
            let pb = crate::pillbox::global();
            let log = SessionLog::open(&pb, "sess-never-written").unwrap();
            assert_eq!(log.last_seq(), 0);
            assert!(log.read_from(0).unwrap().is_empty());
        });
    }

    #[test]
    fn subscribe_drains_from_seq_then_returns_when_stopped() {
        with_isolated_home("log-subscribe-stop", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-sub").unwrap();
            log.append(&[
                Event::session("sess-sub", tool_call("a")),
                Event::session("sess-sub", tool_call("b")),
                Event::session("sess-sub", tool_call("c")),
            ])
            .unwrap();
            // Pre-stopped: subscribe drains the requested tail once, then the
            // stop check returns before any poll-sleep. A late-joiner caught up
            // through seq 1 gets exactly 2 and 3.
            let stop = AtomicBool::new(true);
            let mut seen = Vec::new();
            log.subscribe(2, &stop, |ev| {
                seen.push(ev.seq);
                true
            })
            .unwrap();
            assert_eq!(seen, vec![2, 3]);
        });
    }

    #[test]
    fn subscribe_returns_when_sink_signals_enough() {
        with_isolated_home("log-subscribe-sink-stop", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-sink").unwrap();
            log.append(&[
                Event::session("sess-sink", tool_call("a")),
                Event::session("sess-sink", tool_call("b")),
            ])
            .unwrap();
            // `stop` never trips; the sink returning false is what ends it,
            // after exactly one event — proving the early-out doesn't depend on
            // the stop flag (and never reaches a poll-sleep).
            let stop = AtomicBool::new(false);
            let mut count = 0;
            log.subscribe(0, &stop, |_ev| {
                count += 1;
                false
            })
            .unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn empty_append_is_a_noop() {
        with_isolated_home("log-empty-append", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-5").unwrap();
            assert_eq!(log.append(&[]).unwrap(), 0);
            assert!(log.read_from(0).unwrap().is_empty());
        });
    }
}
