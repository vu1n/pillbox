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
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};

use super::{claude, codex, emit_event_span, Harness, TranscriptEvent};

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
}

impl Tailer {
    pub(crate) fn new(path: PathBuf, session_id: String, harness: Harness) -> Self {
        Self {
            path,
            session_id,
            harness,
            offset: 0,
            line_idx: 0,
            leftover: String::new(),
        }
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

        // Combine leftover + new bytes into a Cow-like view, then
        // split on `\n`. Anything after the final `\n` becomes the
        // new leftover for the next pump.
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
        let mut combined = std::mem::take(&mut self.leftover);
        combined.push_str(text);

        let mut emitted = 0;
        let (complete, partial) = split_trailing_partial(&combined);
        for line in complete.lines() {
            if line.is_empty() {
                continue;
            }
            let events = parse_with(self.harness, line, self.line_idx);
            for event in &events {
                emit_event_span(event, &self.session_id);
                emitted += 1;
            }
            self.line_idx += 1;
        }
        self.leftover = partial.to_string();
        Ok(emitted)
    }

    /// Watch the file with `notify` and re-pump on every modify
    /// event. Blocks until the channel closes (e.g. on Ctrl-C, when
    /// the watcher is dropped by the runtime). Performs an initial
    /// pump first so existing content is drained before tailing.
    ///
    /// Falls back to a polling tick (`debounce`) so a missed
    /// notification — possible on macOS during very fast appends
    /// where coalescing eats events — doesn't strand new lines.
    pub(crate) fn follow(&mut self) -> Result<usize> {
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

        // Block on the channel; poll the file periodically as a
        // safety net.
        let poll = Duration::from_millis(500);
        loop {
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

        let mut tailer = Tailer::new(path.clone(), "sess".into(), Harness::Claude);
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
        let mut tailer = Tailer::new(path.clone(), "sess".into(), Harness::Claude);

        // Write half a line (no trailing \n yet).
        let full = fixture_line("u1", "split-line");
        let mid = full.len() / 2;
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.write_all(full[..mid].as_bytes()).unwrap();
        }
        assert_eq!(tailer.pump().unwrap(), 0, "partial line emits nothing");

        // Now write the rest + the newline.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(full[mid..].as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        assert_eq!(tailer.pump().unwrap(), 1, "completed line emits one event",);
    }

    #[test]
    fn pump_handles_file_truncation_by_rewinding() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let mut tailer = Tailer::new(path.clone(), "sess".into(), Harness::Claude);

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
        let mut tailer = Tailer::new(path, "sess".into(), Harness::Claude);
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
