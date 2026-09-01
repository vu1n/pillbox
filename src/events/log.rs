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
//!   - **live subscribe-follow**: a thin loop over `read_from` + an FS notify,
//!     mirroring [`crate::events::transcripts::Tailer`]. Next slice.
//!   - **producers**: wiring the harness parser (`agents/harness`, which already
//!     produces [`Payload`]s) and the lifecycle stream into the log.
//!   - **`actor` / `class` envelope fields**: land with the gateway (authenticated
//!     actor) and the pooling gate (content-vs-signal) that enforce them.
//!     (`actor` is now stamped by producers; `class` ships per-artifact on the
//!     [`Artifact`](crate::contract::Artifact) payload.)
//!   - **`pty_snapshot` / `raw_body`** and the `head` fast-resume file:
//!     optimizations that arrive with their consumers. (The content-addressed
//!     **blob store** these will use has landed — [`crate::events::blob`] —
//!     driven first by the structured `artifact` payload.)
//!
//! Pure functional core — no docker/agent/network. Fully unit-tested below.

// The log's public surface lands ahead of its first producer (the harness →
// log wiring is the next slice), mirroring contract.rs's contract-first stance.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
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
        Self::open_at(crate::session::session_dir(pb, session_id)?)
    }

    /// Open the local session log without making observability a run blocker.
    /// Transcript capture is best-effort: a failure stays loud, then callers may
    /// continue with OTLP-only observability.
    pub(crate) fn open_or_warn(pb: &Pillbox, session_id: &str) -> Option<Self> {
        match Self::open(pb, session_id) {
            Ok(log) => Some(log),
            Err(error) => {
                eprintln!("pillbox: warning: couldn't open session log: {error:#}");
                None
            }
        }
    }

    /// Open the log at an already-resolved session directory — for a caller that
    /// has the path but not a [`Pillbox`] (the detached §0 producer, re-exec'd as
    /// a bare subprocess, is handed the dir on argv). Same seq recovery as `open`.
    pub(crate) fn open_at(dir: PathBuf) -> Result<Self> {
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
    /// Returns the last `seq` assigned (or the unchanged `last_seq` for an empty
    /// batch).
    ///
    /// Holds an exclusive [`LogLock`] across the read-max + write so concurrent
    /// appenders can't both hand out the same seq. The local §0 spine has genuine
    /// multi-process writers — a `subscribe`/`watch` tailer in one process and
    /// `session send`/`annotate`/`score` in another — and each holds its own
    /// `SessionLog` with an in-memory `last_seq` that goes STALE the moment the
    /// other appends. So under the lock the FILE is the seq authority: re-read its
    /// max via [`recover_last_seq`] rather than trust the cache. (Full re-scan per
    /// append; fine at per-session scale — the byte-offset incremental read is the
    /// deferred optimization, same as `subscribe`. The cross-process single-writer
    /// coordination this lock provides is the cheap stand-in for the resident
    /// sequencer.)
    // Context: doc://pillbox/session-event-log-spine@0001#session-event-log-spine
    pub(crate) fn append(&mut self, events: &[Event]) -> Result<u64> {
        if events.is_empty() {
            return Ok(self.last_seq);
        }
        let path = self.log_path();
        let _lock = LogLock::acquire(&path)?;
        let mut seq = recover_last_seq(&path)?;
        let mut buf = String::new();
        // Heal a torn trailing line: if a prior append crashed mid-write (a partial
        // record with no terminating newline), start the new event on its own line
        // so it can't concatenate onto — and be lost with — the torn fragment (which
        // is itself skipped on read by `fold_parsed_lines`). Surfaced once here (the
        // actionable moment) rather than on every silent read.
        if ends_with_torn_line(&path) {
            eprintln!(
                "pillbox: note: healing a torn trailing line in {} (a prior append \
                 likely crashed mid-write)",
                path.display()
            );
            buf.push('\n');
        }
        for ev in events {
            seq += 1;
            let line = Event { seq, ..ev.clone() };
            buf.push_str(&serde_json::to_string(&line).context("serialize log event")?);
            buf.push('\n');
        }
        append_private_file(&path, buf.as_bytes())?;
        self.last_seq = seq;
        Ok(seq)
    }

    /// Replay every durable event with `seq >= from`. `read_from(0)` is the
    /// full log; `read_from(last_seq + 1)` is "nothing new yet". Returns an
    /// empty vec when the log doesn't exist (a session with no events).
    pub(crate) fn read_from(&self, from: u64) -> Result<Vec<Event>> {
        read_events_at(&self.log_path(), from)
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

/// Read-only replay of a session's durable log **without creating its
/// directory** (unlike [`SessionLog::open`], which `mkdir`s as a write-side
/// side effect). For status derivation that folds many sessions' logs during a
/// read command (`session list` / `diagnose`) — a read must not mutate state,
/// and a remote session whose log lives sandbox-side simply reads empty.
pub(crate) fn read_log(pb: &Pillbox, session_id: &str) -> Result<Vec<Event>> {
    let path = crate::session::session_dir_path(pb, session_id).join(LOG_FILE);
    read_events_at(&path, 0)
}

/// Parse the tail (`seq >= from`) of a log file. Shared by [`SessionLog::
/// read_from`] (writer-side, dir already open) and [`read_log`] (read-only).
fn read_events_at(path: &Path, from: u64) -> Result<Vec<Event>> {
    fold_parsed_lines(path, Vec::new(), |mut out, ev: Event| {
        if ev.seq >= from {
            out.push(ev);
        }
        out
    })
}

/// Fold over the JSONL lines of `path`, deserializing each to `L` and folding the
/// parsed value into the accumulator. Returns `init` unchanged when the file
/// doesn't exist (an empty / never-written log). Shared by replay and seq recovery,
/// which differ only in `L` and the fold.
///
/// A line that fails to deserialize is **skipped**, not fatal: a torn final line
/// (a crash / power-loss / `ENOSPC` mid-append) or a single corrupt line must not
/// brick the whole log — otherwise one bad line would fail every future `append`
/// (via [`recover_last_seq`]) and every reader. Skipping is silent (reads fold over
/// many sessions, so a per-line warning would spam status commands; the common
/// torn-tail case is surfaced once on the append path via [`ends_with_torn_line`]),
/// mirroring the line-tolerant lifecycle fold in [`crate::events::status`].
/// (Unknown/foreign *payloads* aren't skipped — they decode to `Payload::Unknown`;
/// only malformed JSON lands here.)
fn fold_parsed_lines<L, T>(path: &Path, init: T, mut f: impl FnMut(T, L) -> T) -> Result<T>
where
    L: serde::de::DeserializeOwned,
{
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(init),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut acc = init;
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<L>(line) {
            acc = f(acc, parsed);
        }
    }
    Ok(acc)
}

/// True when `path` exists, is non-empty, and does NOT end in a newline — its
/// final line is torn (a crash / power-loss / `ENOSPC` mid-append left a partial
/// record). [`SessionLog::append`] uses this to start the next event on a fresh
/// line. Reads only the last byte (no full scan).
fn ends_with_torn_line(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    // Seek to the last byte; an empty file (or a seek failure) can't be torn.
    if f.seek(SeekFrom::End(-1)).is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    f.read_exact(&mut last).is_ok() && last[0] != b'\n'
}

/// Recover the sequencer position by scanning the log for its maximum `seq`.
/// Parses only the `seq` field (not the full payload) so it stays correct even
/// once newer/foreign envelope shapes exist that this binary can't fully decode —
/// such a line is still counted, so `append` never reuses its seq. This is a
/// superset of what [`read_events_at`] can replay: a counted-but-undecodable line
/// is a benign seq gap, deliberately preferred over parsing the full `Event` here
/// (which would *skip* such a line and then reuse its seq — a duplicate, worse than
/// a gap). A torn/corrupt line is skipped (see [`fold_parsed_lines`]), so the next
/// `append` recovers from the last good line rather than failing forever.
fn recover_last_seq(log_path: &Path) -> Result<u64> {
    #[derive(Deserialize)]
    struct SeqOnly {
        seq: u64,
    }
    fold_parsed_lines(log_path, 0, |max, s: SeqOnly| max.max(s.seq))
}

/// An exclusive advisory lock on a session's log file, held across an append so
/// concurrent appenders serialize (see [`SessionLog::append`]). `flock`-based:
/// the lock is associated with this open file description and the inode, so every
/// `SessionLog::append` — in this process or another — contends on it, and it
/// releases when this fd closes (the `File` drop). `flock` (not POSIX `fcntl`
/// record locks) is deliberate: it's per-fd, so the separate fd
/// [`append_private_file`] opens to write isn't affected, and a stray close
/// elsewhere can't drop our lock.
struct LogLock {
    // Held only for its fd's lifetime; closing it (drop) releases the flock.
    _file: fs::File,
}

impl LogLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("open {} to lock", path.display()))?;
        // Blocks until no other appender holds the lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
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
    fn a_torn_line_does_not_brick_recovery_or_reads() {
        with_isolated_home("log-torn-line", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "sess-torn").unwrap();
            log.append(&[Event::session("sess-torn", tool_call("a"))])
                .unwrap();
            log.append(&[Event::session("sess-torn", tool_call("b"))])
                .unwrap();

            // Simulate a crash / power-loss mid-append: a truncated trailing line.
            let path = crate::session::session_dir_path(&pb, "sess-torn").join(LOG_FILE);
            {
                use std::io::Write;
                let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
                f.write_all(b"{\"seq\":3,\"actor\":{\"par").unwrap();
            }

            // recover_last_seq (run on every append) must SKIP the torn line, not
            // error: a fresh open + append recovers from the last good seq (2) and
            // assigns 3 — the bug was that it bricked append forever.
            let mut log2 = SessionLog::open(&pb, "sess-torn").unwrap();
            let seq = log2
                .append(&[Event::session("sess-torn", tool_call("c"))])
                .unwrap();
            assert_eq!(seq, 3, "append after a torn line must not be bricked");

            // Reads skip the torn line too: the three good events, in seq order.
            let seqs: Vec<u64> = log2.read_from(0).unwrap().iter().map(|e| e.seq).collect();
            assert_eq!(seqs, vec![1, 2, 3]);
        });
    }

    #[test]
    fn concurrent_appenders_get_unique_contiguous_seqs() {
        // Each thread opens its OWN SessionLog (mimicking separate processes — a
        // tailer + `session send`/`annotate`) and appends to the same log. The
        // flock in `append` must serialize them so no seq is duplicated or skipped;
        // without it, each would compute seq from its own stale in-memory last_seq.
        with_isolated_home("log-concurrent-append", || {
            const N: u64 = 16;
            let handles: Vec<_> = (0..N)
                .map(|i| {
                    std::thread::spawn(move || {
                        let pb = crate::pillbox::global();
                        let mut log = SessionLog::open(&pb, "sess-conc").unwrap();
                        log.append(&[Event::session("sess-conc", tool_call(&format!("t{i}")))])
                            .unwrap();
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            let pb = crate::pillbox::global();
            let mut seqs: Vec<u64> = SessionLog::open(&pb, "sess-conc")
                .unwrap()
                .read_from(0)
                .unwrap()
                .iter()
                .map(|e| e.seq)
                .collect();
            seqs.sort_unstable();
            assert_eq!(
                seqs,
                (1..=N).collect::<Vec<_>>(),
                "unique + contiguous, no dup/gap"
            );
        });
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
