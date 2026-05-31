//! Live transcript tailer — drains the file from byte 0 to current
//! length, then blocks waiting on `notify` events and re-pumps
//! whenever the file grows. Same parsers as the one-shot
//! [`super::drain_file_as`] path; this just feeds them line-by-line
//! as the agent harness appends.
//!
//! State carries across pumps:
//! - `offset` — last-known byte position read from the file. We
//!   never re-read what we've already emitted (idempotent vs. a
//!   harness that flushes mid-line by buffering the trailing
//!   partial in `leftover`).
//! - `line_idx` — monotonic across pumps. Codex synthesizes uuids
//!   from it for messages/reasoning; restarting the counter on a
//!   pump would produce duplicate-id spans.
//! - `leftover` — bytes between the last complete `\n` and EOF on
//!   the previous read. Prepended to the next chunk so partial
//!   lines stitch.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{claude, codex, contract_map, emit_event_span, Harness, TranscriptEvent};
use crate::contract::Event;
use crate::events::log::SessionLog;

/// Stateful tail position for one transcript file. Reusable across
/// pumps so partial lines and the line-index counter survive between
/// FS-event wakeups.
pub(crate) struct Tailer {
    path: PathBuf,
    session_id: String,
    harness: Harness,
    offset: u64,
    line_idx: usize,
    leftover: String,
    /// Reconstructs whole-chat gen_ai spans from the events for
    /// Workshop's Overview. Always present — the transcript is the
    /// conversation source for every harness. `include_usage` (threaded
    /// in at construction) controls only whether token counts ride
    /// along, since the vault MITM supplies wire-observed usage for
    /// Claude + `--vault` runs — see [`super::synth`].
    synth: super::synth::ChatSynthesizer,
    /// The durable per-session log this tailer feeds (the spine's first real
    /// producer). `None` when there's no host-side log to write — remote
    /// sandbox-side runs and the manual `session transcript` drain — in which
    /// case the tailer is OTLP-only, as before.
    log: Option<SessionLog>,
}

impl Tailer {
    pub(crate) fn new(
        path: PathBuf,
        session_id: String,
        harness: Harness,
        include_usage: bool,
        log: Option<SessionLog>,
    ) -> Self {
        let synth = super::synth::ChatSynthesizer::new(session_id.clone(), harness, include_usage);
        Self {
            path,
            session_id,
            harness,
            offset: 0,
            line_idx: 0,
            leftover: String::new(),
            synth,
            log,
        }
    }

    /// Construct a tailer with no backing file path — for [`follow_reader`],
    /// where bytes arrive on a pipe rather than a growing file (the docker://
    /// read path). The path field is unused in stream mode; the file-tailing
    /// methods ([`pump`]/[`follow_until`]) are never called on a stream tailer.
    ///
    /// [`follow_reader`]: Tailer::follow_reader
    /// [`pump`]: Tailer::pump
    /// [`follow_until`]: Tailer::follow_until
    pub(crate) fn for_stream(
        session_id: String,
        harness: Harness,
        include_usage: bool,
        log: Option<SessionLog>,
    ) -> Self {
        Self::new(PathBuf::new(), session_id, harness, include_usage, log)
    }

    /// Read any bytes appended since the last pump, parse the
    /// complete lines those bytes produced, emit one span per
    /// parsed event. Returns the number of events emitted.
    ///
    /// Tolerant of two real-world hazards:
    /// - File truncated to shorter than `self.offset` (rare; agent
    ///   harness restarted into the same path). We rewind to 0 and
    ///   reset the partial-line buffer rather than panic.
    /// - File doesn't exist yet at first pump. Returns 0 — the
    ///   caller's notify watch will retry as soon as it appears.
    pub(crate) fn pump(&mut self) -> Result<usize> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).with_context(|| format!("open {}", self.path.display())),
        };
        let len = file
            .metadata()
            .with_context(|| format!("stat {}", self.path.display()))?
            .len();
        if len < self.offset {
            // File rotated / truncated under us; rewind so we don't
            // miss the head of the new content.
            self.offset = 0;
            self.leftover.clear();
        }
        if len == self.offset {
            return Ok(0);
        }
        file.seek(SeekFrom::Start(self.offset))
            .with_context(|| format!("seek {}", self.path.display()))?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        file.read_to_end(&mut buf)
            .with_context(|| format!("read {}", self.path.display()))?;
        self.offset = len;

        let text = match std::str::from_utf8(&buf) {
            Ok(s) => s,
            Err(_) => {
                // Non-UTF8 input shouldn't happen on JSONL but if it
                // does, skip this chunk rather than corrupt the
                // parser state. The next pump's read will give us a
                // fresh window past `self.offset`.
                self.leftover.clear();
                return Ok(0);
            }
        };
        self.ingest(text)
    }

    /// Parse the complete lines in `chunk` (prepended with any leftover partial
    /// line from a previous call), emitting one span + durable event per parsed
    /// transcript event; the trailing partial line is buffered for next time.
    /// The byte-level reader — file ([`Tailer::pump`]) or pipe
    /// ([`Tailer::follow_reader`]) — owns *where* bytes come from; this owns
    /// *what they mean*.
    fn ingest(&mut self, chunk: &str) -> Result<usize> {
        // Combine leftover + new bytes, then split on `\n`. Anything after the
        // final `\n` becomes the new leftover for the next call.
        let mut combined = std::mem::take(&mut self.leftover);
        combined.push_str(chunk);

        let mut emitted = 0;
        // Accumulate this pump's durable events and append them in one write
        // (one file open per pump, not per event) — matters on the initial
        // drain / catch-up burst. OTLP + synth stay per-event (independent).
        let logging = self.log.is_some();
        let mut durable: Vec<Event> = Vec::new();
        let (complete, partial) = split_trailing_partial(&combined);
        for line in complete.lines() {
            if line.is_empty() {
                continue;
            }
            let events = parse_with(self.harness, line, self.line_idx);
            for event in &events {
                if logging {
                    durable.extend(
                        contract_map::to_payloads(event)
                            .into_iter()
                            .map(|p| Event::session(&self.session_id, p)),
                    );
                }
                emit_event_span(event, &self.session_id);
                self.synth.on_event(event);
                emitted += 1;
            }
            self.line_idx += 1;
        }
        // Durable spine append — best-effort + loud: a write failure must not
        // strand the OTLP/synth emits above or the tail's progress. (`append`
        // is a no-op on an empty batch.)
        if let Some(log) = &mut self.log {
            if let Err(e) = log.append(&durable) {
                eprintln!("pillbox: warning: session log append failed: {e:#}");
            }
        }
        self.leftover = partial.to_string();
        Ok(emitted)
    }

    /// Watch the file with `notify` and re-pump on every modify
    /// event. Blocks until the channel closes (e.g. on Ctrl-C, when
    /// the watcher is dropped by the runtime). Performs an initial
    /// pump first so existing content is drained before tailing.
    ///
    /// The CLI `--follow` path runs until the process is signalled;
    /// the in-process local tailer wants a clean stop when the agent
    /// exits, so this delegates to [`Tailer::follow_until`] with a
    /// flag that never trips.
    pub(crate) fn follow(&mut self) -> Result<usize> {
        let never = AtomicBool::new(false);
        self.follow_until(&never)
    }

    /// Like [`Tailer::follow`], but also returns once `stop` is set —
    /// after a final [`Tailer::pump`] so the agent's last appended
    /// lines aren't stranded. The in-process local-docker tailer flips
    /// `stop` when the agent exits.
    ///
    /// Falls back to a polling tick so a missed notification —
    /// possible on macOS during very fast appends where coalescing
    /// eats events, or a stop signalled while we're blocked on
    /// `recv` — doesn't strand new lines or wedge teardown.
    pub(crate) fn follow_until(&mut self, stop: &AtomicBool) -> Result<usize> {
        use notify::{RecursiveMode, Watcher};

        let mut total = self.pump()?;

        let (tx, rx) = mpsc::channel();
        // Watching the *parent* dir rather than the file directly so
        // the watcher survives if the harness atomically renames
        // (some loggers write to a tempfile + rename on rotation).
        let watch_dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .context("build notify watcher")?;
        watcher
            .watch(&watch_dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("watch {}", watch_dir.display()))?;

        // Block on the channel; poll the file periodically both as a
        // safety net for missed FS events and to bound how long a
        // `stop` request waits (≤ one poll interval).
        let poll = Duration::from_millis(500);
        loop {
            if stop.load(Ordering::Relaxed) {
                // Final drain: catch anything the agent flushed between
                // the last pump and exit.
                total += self.pump()?;
                break;
            }
            match rx.recv_timeout(poll) {
                Ok(Ok(_event)) => {
                    total += self.pump()?;
                }
                Ok(Err(e)) => {
                    eprintln!("pillbox: warning: watcher error: {e}; continuing");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    total += self.pump()?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }
        // Emit the last assistant turn (it's only flushed lazily on the
        // next user prompt, which never comes for the final exchange).
        self.synth.finish();
        Ok(total)
    }

    /// Drain a *streamed* transcript — bytes arriving on a pipe rather than a
    /// growing file. The docker:// read path tails the container's transcript
    /// out over the endpoint (`docker exec … tail -F`) and feeds the child's
    /// stdout here, so a remote session fills the host's durable log exactly
    /// like a local bind-mounted one — same parser, same `SessionLog`.
    ///
    /// Blocks reading the pipe until it closes (the remote `tail` ends / the
    /// container stops) or `stop` is set. The blocking read is interruptible
    /// only at chunk boundaries; callers that need a hard stop close the
    /// underlying child (the read then returns EOF).
    pub(crate) fn follow_reader(
        &mut self,
        mut reader: impl std::io::Read,
        stop: &AtomicBool,
    ) -> Result<usize> {
        let mut total = 0;
        let mut buf = [0u8; 8192];
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let n = reader
                .read(&mut buf)
                .context("read remote transcript stream")?;
            if n == 0 {
                break; // pipe closed
            }
            match std::str::from_utf8(&buf[..n]) {
                Ok(s) => total += self.ingest(s)?,
                // A read can split a multi-byte char; the leftover-line buffer
                // can't stitch mid-UTF8, so drop this chunk rather than corrupt
                // the parse. Rare on ASCII-dominant JSONL; the next line resyncs.
                Err(_) => self.leftover.clear(),
            }
        }
        self.synth.finish();
        Ok(total)
    }
}

fn parse_with(harness: Harness, line: &str, idx: usize) -> Vec<TranscriptEvent> {
    match harness {
        Harness::Claude => claude::parse_line(line, idx),
        Harness::Codex => codex::parse_line(line, idx),
    }
}

/// Split `s` into `(complete, partial)` where `complete` ends at the
/// last `\n` and `partial` is whatever followed. If `s` has no `\n`
/// the whole thing is partial — we haven't seen a full line yet.
fn split_trailing_partial(s: &str) -> (&str, &str) {
    match s.rfind('\n') {
        Some(idx) => s.split_at(idx + 1),
        None => ("", s),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn fixture_line(uuid: &str, content: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":null,"timestamp":"2026-05-28T10:00:00Z","message":{{"role":"user","content":"{content}"}}}}"#,
        )
    }

    /// The producer wiring end-to-end: a pumped transcript line lands in the
    /// durable per-session log as the mapped contract payloads (a user prompt →
    /// the MessageStart/Delta/End triple), readable back via a fresh handle.
    #[test]
    fn pump_feeds_the_durable_session_log() {
        crate::test_util::with_isolated_home("tailer-feeds-log", || {
            use crate::contract::Payload;
            let pb = crate::pillbox::global();
            let log = SessionLog::open(&pb, "sess-tail").expect("open log");

            let tmp = tempfile::NamedTempFile::new().expect("tempfile");
            let path = tmp.path().to_path_buf();
            {
                let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                writeln!(f, "{}", fixture_line("u1", "hello")).unwrap();
            }

            let mut tailer =
                Tailer::new(path, "sess-tail".into(), Harness::Claude, false, Some(log));
            assert_eq!(tailer.pump().expect("pump"), 1, "one transcript event");

            // Read the log back through a fresh handle (appends are flushed).
            let events = SessionLog::open(&pb, "sess-tail")
                .unwrap()
                .read_from(0)
                .unwrap();
            assert_eq!(events.len(), 3, "user prompt → start/delta/end");
            assert_eq!(events[0].session_id, "sess-tail");
            assert!(matches!(events[0].payload, Payload::MessageStart(_)));
            assert!(matches!(&events[1].payload, Payload::MessageDelta(d) if d.text == "hello"));
            assert!(matches!(events[2].payload, Payload::MessageEnd(_)));
            // The log assigned the per-session seq, not the producer.
            assert_eq!(
                events.iter().map(|e| e.seq).collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
        });
    }

    /// The docker:// read path end-to-end: a streamed transcript (bytes off a
    /// pipe, here a `Cursor`) drains through `follow_reader` into the durable
    /// log as the mapped payloads — same result as the file-based `pump`, and
    /// partial lines split mid-stream still stitch.
    #[test]
    fn follow_reader_drains_a_streamed_transcript_into_the_log() {
        crate::test_util::with_isolated_home("tailer-follow-reader", || {
            use std::io::Cursor;
            use std::sync::atomic::AtomicBool;

            let pb = crate::pillbox::global();
            let log = SessionLog::open(&pb, "sess-stream").expect("open log");

            // Two complete lines + a chunk boundary in the middle of the second
            // (the Cursor hands them all at once, but ingest's leftover buffer is
            // what stitches a split — exercised by pump's partial-line test; here
            // we assert the stream path maps both lines).
            let mut stream = String::new();
            stream.push_str(&fixture_line("u1", "alpha"));
            stream.push('\n');
            stream.push_str(&fixture_line("u2", "beta"));
            stream.push('\n');

            let mut tailer =
                Tailer::for_stream("sess-stream".into(), Harness::Claude, false, Some(log));
            let stop = AtomicBool::new(false);
            let n = tailer
                .follow_reader(Cursor::new(stream), &stop)
                .expect("follow_reader");
            assert_eq!(n, 2, "two streamed transcript events");

            let events = SessionLog::open(&pb, "sess-stream")
                .unwrap()
                .read_from(0)
                .unwrap();
            // Each user prompt → start/delta/end triple.
            assert_eq!(events.len(), 6);
            assert!(matches!(&events[1].payload,
                crate::contract::Payload::MessageDelta(d) if d.text == "alpha"));
            assert!(matches!(&events[4].payload,
                crate::contract::Payload::MessageDelta(d) if d.text == "beta"));
            assert_eq!(
                events.iter().map(|e| e.seq).collect::<Vec<_>>(),
                vec![1, 2, 3, 4, 5, 6]
            );
        });
    }

    #[test]
    fn pump_drains_existing_lines_then_emits_only_appended() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();

        // Seed two lines before the tailer starts.
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            writeln!(f, "{}", fixture_line("u1", "first")).unwrap();
            writeln!(f, "{}", fixture_line("u2", "second")).unwrap();
        }

        let mut tailer = Tailer::new(path.clone(), "sess".into(), Harness::Claude, false, None);
        let first = tailer.pump().expect("pump");
        assert_eq!(first, 2, "initial pump should drain both seeded lines");

        // Second pump with no new bytes: zero events.
        let none = tailer.pump().expect("pump");
        assert_eq!(none, 0);

        // Append a third line; pump should pick it up.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{}", fixture_line("u3", "third")).unwrap();
        }
        let one = tailer.pump().expect("pump");
        assert_eq!(one, 1);
    }

    #[test]
    fn pump_buffers_partial_lines_across_calls() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let mut tailer = Tailer::new(path.clone(), "sess".into(), Harness::Claude, false, None);

        // Write half a line (no trailing \n yet).
        let full = fixture_line("u1", "split-line");
        let mid = full.len() / 2;
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.write_all(&full.as_bytes()[..mid]).unwrap();
        }
        assert_eq!(tailer.pump().unwrap(), 0, "partial line emits nothing");

        // Now write the rest + the newline.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&full.as_bytes()[mid..]).unwrap();
            f.write_all(b"\n").unwrap();
        }
        assert_eq!(tailer.pump().unwrap(), 1, "completed line emits one event",);
    }

    #[test]
    fn pump_handles_file_truncation_by_rewinding() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let mut tailer = Tailer::new(path.clone(), "sess".into(), Harness::Claude, false, None);

        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            writeln!(f, "{}", fixture_line("u1", "before-truncate")).unwrap();
        }
        assert_eq!(tailer.pump().unwrap(), 1);

        // Truncate the file (simulates a harness restart that
        // recreates the same path) and write fresh content.
        std::fs::File::create(&path).unwrap();
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            writeln!(f, "{}", fixture_line("u2", "after-truncate")).unwrap();
        }
        // Without rewind, the pump would skip the new line because
        // offset > new file size.
        assert_eq!(tailer.pump().unwrap(), 1);
    }

    #[test]
    fn pump_handles_missing_file_silently() {
        let path = PathBuf::from(format!(
            "/tmp/pillbox-tailer-missing-{}.jsonl",
            uuid::Uuid::now_v7(),
        ));
        let mut tailer = Tailer::new(path, "sess".into(), Harness::Claude, false, None);
        assert_eq!(tailer.pump().unwrap(), 0);
    }

    #[test]
    fn split_trailing_partial_isolates_unterminated_tail() {
        let (c, p) = split_trailing_partial("a\nb\nc");
        assert_eq!(c, "a\nb\n");
        assert_eq!(p, "c");

        let (c, p) = split_trailing_partial("");
        assert_eq!(c, "");
        assert_eq!(p, "");

        let (c, p) = split_trailing_partial("complete\n");
        assert_eq!(c, "complete\n");
        assert_eq!(p, "");

        let (c, p) = split_trailing_partial("only-partial");
        assert_eq!(c, "");
        assert_eq!(p, "only-partial");
    }
}
