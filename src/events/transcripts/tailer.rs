//! Live transcript tailer — drains from its in-memory or durable byte
//! position, then blocks waiting on `notify` events and re-pumps whenever
//! the file grows. Same parsers as the one-shot
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

use std::io::{Read, Seek, SeekFrom, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{claude, codex, contract_map, emit_event_span, Harness, TranscriptEvent};
use crate::contract::{Actor, Event};
use crate::events::log::SessionLog;

const CURSOR_VERSION: u8 = 1;
const CODEX_CURSOR_VERSION: u8 = 2;
const TRANSCRIPT_READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_LINE_BYTES: usize = 1024 * 1024;

type ParsedBatch = (
    Vec<TranscriptEvent>,
    Vec<Event>,
    usize,
    Option<codex::Parser>,
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CursorPosition {
    transcript: PathBuf,
    device: u64,
    inode: u64,
    offset: u64,
    line_idx: usize,
    /// Codex cumulative-usage baseline. Claude cursors leave this absent so
    /// existing v1 Claude sessions remain readable.
    #[serde(default)]
    codex_parser: Option<codex::ParserState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum CursorState {
    Committed {
        version: u8,
        position: CursorPosition,
    },
    Committing {
        version: u8,
        position: CursorPosition,
        pre_append_seq: u64,
        events: Vec<Event>,
        events_sha256: String,
    },
}

impl CursorState {
    fn version(&self) -> u8 {
        match self {
            Self::Committed { version, .. } | Self::Committing { version, .. } => *version,
        }
    }

    fn position(&self) -> &CursorPosition {
        match self {
            Self::Committed { position, .. } | Self::Committing { position, .. } => position,
        }
    }

    fn validate(&self, session_id: &str) -> Result<()> {
        if !matches!(self.version(), CURSOR_VERSION | CODEX_CURSOR_VERSION)
            || self.position().transcript.as_os_str().is_empty()
        {
            anyhow::bail!("invalid detached transcript cursor identity or unsupported version");
        }
        if let Self::Committing {
            events,
            events_sha256,
            ..
        } = self
        {
            if events.is_empty()
                || events.iter().any(|event| event.session_id != session_id)
                || *events_sha256 != hash_events(events)?
            {
                anyhow::bail!("invalid detached transcript cursor commit intent");
            }
        }
        Ok(())
    }
}

struct DurableCursor {
    path: PathBuf,
    position: CursorPosition,
    version: u8,
}

impl DurableCursor {
    fn source(path: &Path, session_id: &str) -> Result<Option<PathBuf>> {
        Ok(read_cursor(path, session_id)?.map(|state| state.position().transcript.clone()))
    }

    fn open(
        path: PathBuf,
        transcript: &Path,
        session_id: &str,
        log: &mut SessionLog,
        expected_version: u8,
    ) -> Result<Self> {
        let (device, inode) = source_identity(transcript)?;
        let state = read_cursor(&path, session_id)?;
        let initializing = state.is_none();
        if state.as_ref().is_some_and(|state| {
            let position = state.position();
            position.transcript != transcript
                || position.device != device
                || position.inode != inode
        }) {
            anyhow::bail!(
                "detached transcript cursor does not match rollout {}",
                transcript.display()
            );
        }
        let state_version = state.as_ref().map(CursorState::version);
        if let Some(version) = state_version.filter(|version| *version != expected_version) {
            anyhow::bail!(
                "detached transcript cursor version {version} is incompatible with this {} producer (expected {expected_version}); preserve the cursor and start a new session",
                if expected_version == CODEX_CURSOR_VERSION {
                    "Codex"
                } else {
                    "Claude"
                }
            );
        }
        let position = match state {
            None => CursorPosition {
                transcript: transcript.to_path_buf(),
                device,
                inode,
                offset: 0,
                line_idx: 0,
                codex_parser: None,
            },
            Some(CursorState::Committed { position, .. }) => position,
            Some(CursorState::Committing {
                position,
                pre_append_seq,
                events,
                ..
            }) => {
                log.append_exact_batch(&events, Some(pre_append_seq), |_| {
                    anyhow::bail!("recovering transcript cursor was unexpectedly prepared twice")
                })?;
                persist_cursor(
                    &path,
                    &CursorState::Committed {
                        version: expected_version,
                        position: position.clone(),
                    },
                )?;
                position
            }
        };
        if initializing {
            persist_cursor(
                &path,
                &CursorState::Committed {
                    version: expected_version,
                    position: position.clone(),
                },
            )?;
        }
        Ok(Self {
            path,
            position,
            version: expected_version,
        })
    }

    fn commit_batch(
        &mut self,
        log: &mut SessionLog,
        events: &[Event],
        next: CursorPosition,
    ) -> Result<()> {
        if events.is_empty() {
            persist_cursor(
                &self.path,
                &CursorState::Committed {
                    version: self.version,
                    position: next.clone(),
                },
            )?;
        } else {
            log.append_exact_batch(events, None, |pre_append_seq| {
                persist_cursor(
                    &self.path,
                    &CursorState::Committing {
                        version: self.version,
                        position: next.clone(),
                        pre_append_seq,
                        events: events.to_vec(),
                        events_sha256: hash_events(events)?,
                    },
                )
            })?;
            persist_cursor(
                &self.path,
                &CursorState::Committed {
                    version: self.version,
                    position: next.clone(),
                },
            )?;
        }
        self.position = next;
        Ok(())
    }
}

fn read_cursor(path: &Path, session_id: &str) -> Result<Option<CursorState>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let state: CursorState = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode detached transcript cursor {}", path.display()))?;
    state.validate(session_id)?;
    Ok(Some(state))
}

fn persist_cursor(path: &Path, state: &CursorState) -> Result<()> {
    let bytes = serde_json::to_vec(state).context("serialize detached transcript cursor")?;
    let dir = path
        .parent()
        .context("detached transcript cursor has no parent directory")?;
    let mut temp = tempfile::Builder::new()
        .prefix(".tailer-cursor-")
        .tempfile_in(dir)
        .with_context(|| format!("create detached transcript cursor in {}", dir.display()))?;
    temp.as_file_mut()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .context("chmod detached transcript cursor 0600")?;
    temp.write_all(&bytes)
        .context("write detached transcript cursor")?;
    temp.as_file()
        .sync_all()
        .context("fsync detached transcript cursor")?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("persist {}", path.display()))?;
    std::fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("fsync {}", dir.display()))
}

fn source_identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat detached transcript {}", path.display()))?;
    if !metadata.file_type().is_file() {
        anyhow::bail!(
            "detached transcript is not a regular file: {}",
            path.display()
        );
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn hash_events(events: &[Event]) -> Result<String> {
    let bytes = serde_json::to_vec(events).context("serialize transcript cursor event batch")?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

/// Stateful tail position for one transcript file. Reusable across
/// pumps so partial lines and the line-index counter survive between
/// FS-event wakeups.
pub(crate) struct Tailer {
    path: PathBuf,
    session_id: String,
    harness: Harness,
    offset: u64,
    line_idx: usize,
    leftover: Vec<u8>,
    /// Reconstructs whole-chat gen_ai spans from the events for
    /// Workshop's Overview. Always present — the transcript is the
    /// conversation source for every harness. `include_usage` (threaded
    /// in at construction) controls only whether token counts ride
    /// along, since the vault MITM supplies wire-observed usage for
    /// Claude + `--vault` runs — see [`super::synth`].
    synth: super::synth::ChatSynthesizer,
    /// The durable local §0 sink this tailer feeds. `None` means OTLP-only
    /// observability, used when opening the best-effort log failed or the manual
    /// `session transcript` drain has no session log.
    log: Option<SessionLog>,
    /// Present only for the reparented Codex producer. It turns transcript
    /// consumption + §0 append into a recoverable local transaction.
    cursor: Option<DurableCursor>,
    /// Stateful only for Codex: cumulative token baselines are persisted in
    /// `cursor.position.codex_parser` before the transcript cursor advances.
    codex_parser: Option<codex::Parser>,
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
            leftover: Vec::new(),
            synth,
            log,
            cursor: None,
            codex_parser: (harness == Harness::Codex).then(codex::Parser::default),
        }
    }

    pub(crate) fn durable_source(cursor_path: &Path, session_id: &str) -> Result<Option<PathBuf>> {
        DurableCursor::source(cursor_path, session_id)
    }

    pub(crate) fn new_durable(
        path: PathBuf,
        session_id: String,
        harness: Harness,
        include_usage: bool,
        mut log: SessionLog,
        cursor_path: PathBuf,
    ) -> Result<Self> {
        let expected_version = if harness == Harness::Codex {
            CODEX_CURSOR_VERSION
        } else {
            CURSOR_VERSION
        };
        let cursor =
            DurableCursor::open(cursor_path, &path, &session_id, &mut log, expected_version)?;
        if harness == Harness::Codex
            && cursor.position.offset > 0
            && cursor.position.codex_parser.is_none()
        {
            anyhow::bail!(
                "Codex detached transcript cursor has no accounting baseline at offset {}; preserve the cursor and start a new session",
                cursor.position.offset
            );
        }
        let codex_parser = (harness == Harness::Codex)
            .then(|| codex::Parser::from_state(cursor.position.codex_parser.clone()));
        let synth = super::synth::ChatSynthesizer::new(session_id.clone(), harness, include_usage);
        Ok(Self {
            path,
            session_id,
            harness,
            offset: cursor.position.offset,
            line_idx: cursor.position.line_idx,
            leftover: Vec::new(),
            synth,
            log: Some(log),
            cursor: Some(cursor),
            codex_parser,
        })
    }

    /// Read any bytes appended since the last pump, parse the
    /// complete lines those bytes produced, emit one span per
    /// parsed event. Returns the number of events emitted.
    ///
    /// The ordinary in-process tailer tolerates two real-world hazards:
    /// - File truncated to shorter than `self.offset` (rare; agent
    ///   harness restarted into the same path). It rewinds to 0 and
    ///   resets the partial-line buffer; a durable producer fails loud because
    ///   replaying from zero would duplicate accepted evidence.
    /// - File doesn't exist yet at first pump. Returns 0 — the
    ///   caller's notify watch will retry as soon as it appears.
    pub(crate) fn pump(&mut self) -> Result<usize> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).with_context(|| format!("open {}", self.path.display())),
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("stat {}", self.path.display()))?;
        if self.cursor.as_ref().is_some_and(|cursor| {
            metadata.dev() != cursor.position.device || metadata.ino() != cursor.position.inode
        }) {
            anyhow::bail!(
                "detached transcript {} changed file identity behind its durable cursor",
                self.path.display()
            );
        }
        let len = metadata.len();
        if len < self.offset {
            if self.cursor.is_some() {
                anyhow::bail!(
                    "detached transcript {} was truncated behind its durable cursor",
                    self.path.display()
                );
            }
            // File rotated / truncated under us; rewind so we don't
            // miss the head of the new content.
            self.offset = 0;
            self.line_idx = 0;
            self.leftover.clear();
            self.codex_parser = (self.harness == Harness::Codex).then(codex::Parser::default);
        }
        if len == self.offset {
            return Ok(0);
        }
        file.seek(SeekFrom::Start(self.offset))
            .with_context(|| format!("seek {}", self.path.display()))?;
        let mut emitted = 0;
        while self.offset < len {
            let read_len = (len - self.offset).min(TRANSCRIPT_READ_CHUNK_BYTES as u64);
            let mut buf = Vec::with_capacity(read_len as usize);
            (&mut file)
                .take(read_len)
                .read_to_end(&mut buf)
                .with_context(|| format!("read {}", self.path.display()))?;
            if buf.is_empty() {
                break;
            }
            self.offset = self
                .offset
                .checked_add(buf.len() as u64)
                .context("transcript read offset overflow")?;
            emitted += self.ingest_bytes(&buf)?;
        }
        Ok(emitted)
    }

    /// Buffer one bounded read chunk, then parse and commit every complete line.
    /// Bytes after the last LF stay in memory so an unterminated record is never
    /// reread by the active producer and a UTF-8 code point may span chunks.
    fn ingest_bytes(&mut self, chunk: &[u8]) -> Result<usize> {
        self.leftover.extend_from_slice(chunk);
        validate_record_lengths(&self.leftover, &self.path)?;
        let Some(last_newline) = self.leftover.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(0);
        };
        let mut complete = std::mem::take(&mut self.leftover);
        self.leftover = complete.split_off(last_newline + 1);
        let complete = std::str::from_utf8(&complete)
            .with_context(|| format!("decode transcript {} as UTF-8", self.path.display()))?;

        if self.cursor.is_some() {
            let next_offset = self
                .offset
                .checked_sub(self.leftover.len() as u64)
                .context("detached transcript cursor offset underflow")?;
            return self.ingest_durable(complete, next_offset);
        }

        let (parsed, durable, next_line_idx, next_codex_parser) = self.parse_complete(complete)?;
        // Durable spine append — best-effort + loud: a write failure must not
        // strand the OTLP/synth emits above or the tail's progress. (`append`
        // is a no-op on an empty batch.)
        if let Some(log) = &mut self.log {
            if let Err(e) = log.append(&durable) {
                eprintln!("pillbox: warning: session log append failed: {e:#}");
            }
        }
        self.codex_parser = next_codex_parser;
        self.emit_parsed(&parsed);
        self.line_idx = next_line_idx;
        Ok(parsed.len())
    }

    fn ingest_durable(&mut self, complete: &str, next_offset: u64) -> Result<usize> {
        let (parsed, durable, next_line_idx, next_codex_parser) = self.parse_complete(complete)?;
        let cursor = self.cursor.as_mut().expect("durable ingest has a cursor");
        let next = CursorPosition {
            transcript: cursor.position.transcript.clone(),
            device: cursor.position.device,
            inode: cursor.position.inode,
            offset: next_offset,
            line_idx: next_line_idx,
            codex_parser: next_codex_parser.as_ref().map(codex::Parser::state),
        };
        cursor.commit_batch(
            self.log.as_mut().expect("durable cursor requires a log"),
            &durable,
            next,
        )?;
        self.codex_parser = next_codex_parser;
        self.line_idx = next_line_idx;
        self.emit_parsed(&parsed);
        Ok(parsed.len())
    }

    fn parse_complete(&self, complete: &str) -> Result<ParsedBatch> {
        let mut parsed = Vec::new();
        let mut durable = Vec::new();
        let mut line_idx = self.line_idx;
        // Parse against a copy so a rejected batch cannot advance accounting.
        let mut codex_parser = self.codex_parser.clone();
        for line in complete.lines() {
            if line.is_empty() {
                continue;
            }
            let events = match self.harness {
                Harness::Claude => claude::parse_line(line, line_idx),
                Harness::Codex => codex_parser
                    .as_mut()
                    .expect("Codex tailer parser")
                    .parse_line_checked(line, line_idx)
                    .with_context(|| format!("parse Codex transcript line {line_idx}"))?,
            };
            for event in &events {
                if self.log.is_some() {
                    durable.extend(contract_map::to_payloads(event).into_iter().map(|payload| {
                        Event::session(&self.session_id, payload)
                            .with_actor(Actor::agent(self.harness.agent_id()))
                    }));
                }
            }
            parsed.extend(events);
            line_idx = line_idx
                .checked_add(1)
                .context("transcript line index overflow")?;
        }
        Ok((parsed, durable, line_idx, codex_parser))
    }

    fn emit_parsed(&mut self, parsed: &[TranscriptEvent]) {
        for event in parsed {
            emit_event_span(event, &self.session_id, self.harness);
            self.synth.on_event(event);
        }
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
}

#[cfg(test)]
fn parse_with(harness: Harness, line: &str, idx: usize) -> Vec<TranscriptEvent> {
    match harness {
        Harness::Claude => claude::parse_line(line, idx),
        Harness::Codex => codex::parse_line(line, idx),
    }
}

fn validate_record_lengths(bytes: &[u8], path: &Path) -> Result<()> {
    for record in bytes.split(|byte| *byte == b'\n') {
        if record.len() > MAX_TRANSCRIPT_LINE_BYTES {
            anyhow::bail!(
                "transcript record in {} exceeds {MAX_TRANSCRIPT_LINE_BYTES} bytes",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::events::log::SessionLog;

    fn fixture_line(uuid: &str, content: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":null,"timestamp":"2026-05-28T10:00:00Z","message":{{"role":"user","content":"{content}"}}}}"#,
        )
    }

    fn codex_complete(turn: &str, content: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-05-18T09:26:31Z","type":"event_msg","payload":{{"type":"task_complete","turn_id":"{turn}","last_agent_message":"{content}"}}}}"#,
        )
    }

    fn codex_custom_call(call_id: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-09-08T06:39:44Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "input": "const r = await tools.exec_command({cmd:\"pwd\"});",
                "call_id": call_id,
            }
        })
        .to_string()
    }

    fn codex_custom_output(call_id: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-09-08T06:39:44Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": call_id,
                "output": [
                    {"type": "input_text", "text": "Script completed\n"},
                    {"type": "input_text", "text": "SAFE_OUTPUT\n"},
                ],
            }
        })
        .to_string()
    }

    fn codex_tokens(input: u64, cached: u64, output: u64) -> String {
        serde_json::json!({
            "timestamp": "2026-09-08T06:39:44Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "cache_write_input_tokens": 0,
                        "output_tokens": output,
                    },
                    "model_context_window": 258400,
                },
            }
        })
        .to_string()
    }

    const BURNIN_USAGE: [(u64, u64, u64); 9] = [
        (11_574, 9_984, 487),
        (23_671, 19_968, 624),
        (35_928, 30_976, 725),
        (48_332, 43_008, 926),
        (60_975, 55_040, 962),
        (73_736, 66_048, 1_213),
        (86_776, 78_080, 1_317),
        (99_943, 90_112, 1_406),
        (113_226, 102_144, 1_425),
    ];

    fn codex_burnin_lines() -> Vec<String> {
        let mut lines = Vec::new();
        for (index, (input, cached, output)) in BURNIN_USAGE.iter().copied().enumerate() {
            if index < 4 {
                let call_id = format!("call-first-{index}");
                lines.push(codex_custom_call(&call_id));
                lines.push(codex_custom_output(&call_id));
            } else if (5..8).contains(&index) {
                let call_id = format!("call-second-{}", index - 5);
                lines.push(codex_custom_call(&call_id));
                lines.push(codex_custom_output(&call_id));
            }
            lines.push(codex_tokens(input, cached, output));
            if index == 4 {
                lines.push(codex_complete("turn-one", "FIRST_DONE"));
            }
        }
        lines.push(codex_complete("turn-two", "SECOND_DONE"));
        lines
    }

    fn append_lines(path: &Path, lines: &[String]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open transcript for append");
        for line in lines {
            writeln!(file, "{line}").expect("append transcript line");
        }
    }

    fn assert_burnin_projection(events: &[crate::contract::Event]) {
        use crate::contract::Payload;

        let tool_calls = events
            .iter()
            .filter_map(|event| match &event.payload {
                Payload::ToolCall(tool) => Some(tool),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tool_calls
                .iter()
                .filter(|tool| tool.status == crate::contract::ToolStatus::Running)
                .count(),
            7,
            "one running event per modern custom tool call"
        );
        assert_eq!(
            tool_calls
                .iter()
                .filter(|tool| tool.status == crate::contract::ToolStatus::Completed)
                .count(),
            7,
            "one completed event per modern custom tool output"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.payload, Payload::Usage(_)))
                .count(),
            9,
            "one incremental usage event per changed cumulative snapshot"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.payload, Payload::AttentionRequired(_)))
                .count(),
            2,
            "task_complete remains the only idle boundary"
        );

        let cost = crate::cost::RunCostEnvelope::from_events(events);
        assert_eq!(cost.model.input_tokens, 11_082);
        assert_eq!(cost.model.cache_read_input_tokens, 102_144);
        assert_eq!(cost.model.output_tokens, 1_425);
    }

    #[test]
    fn durable_codex_burnin_fixture_replays_tools_and_incremental_usage() {
        crate::test_util::with_isolated_home("tailer-codex-burnin-fixture", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-codex-burnin";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout.jsonl");
            let lines = codex_burnin_lines();
            // First turn (four tools + five cumulative snapshots), then one
            // second-turn snapshot before the producer is replaced.
            let first_turn_end = 14;
            let first_second_snapshot_end = 17;
            append_lines(&transcript, &lines[..first_turn_end]);

            let mut first = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(first.pump().unwrap(), 14);
            append_lines(
                &transcript,
                &lines[first_turn_end..first_second_snapshot_end],
            );
            assert_eq!(first.pump().unwrap(), 3);
            drop(first);

            // The replacement sees an unchanged cumulative snapshot first;
            // it must not emit a duplicate usage event before consuming the
            // remaining second-turn records.
            append_lines(
                &transcript,
                &[codex_tokens(
                    BURNIN_USAGE[5].0,
                    BURNIN_USAGE[5].1,
                    BURNIN_USAGE[5].2,
                )],
            );
            append_lines(&transcript, &lines[first_second_snapshot_end..]);
            let mut replacement = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(replacement.pump().unwrap(), 8);
            drop(replacement);

            // A clean replay from the committed cursor has no work left.
            let mut replay = Tailer::new_durable(
                transcript,
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path,
            )
            .unwrap();
            assert_eq!(replay.pump().unwrap(), 0);

            let events = SessionLog::open(&pb, session_id)
                .unwrap()
                .read_from(0)
                .unwrap();
            assert_burnin_projection(&events);
        });
    }

    #[test]
    fn durable_prepared_usage_batch_recovers_without_replay() {
        crate::test_util::with_isolated_home("tailer-durable-prepared-usage", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-prepared-usage";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout.jsonl");
            let line = codex_tokens(1_000, 800, 10);
            std::fs::write(&transcript, format!("{line}\n")).unwrap();
            let mut parser = codex::Parser::default();
            let parsed = parser.parse_line_checked(&line, 0).unwrap();
            assert!(matches!(
                parsed.as_slice(),
                [TranscriptEvent {
                    kind: super::super::EventKind::Usage { .. },
                    ..
                }]
            ));
            let events = parsed
                .iter()
                .flat_map(contract_map::to_payloads)
                .map(|payload| {
                    Event::session(session_id, payload)
                        .with_actor(Actor::agent(Harness::Codex.agent_id()))
                })
                .collect::<Vec<_>>();
            let (device, inode) = source_identity(&transcript).unwrap();
            let position = CursorPosition {
                transcript,
                device,
                inode,
                offset: line.len() as u64 + 1,
                line_idx: 1,
                codex_parser: Some(parser.state()),
            };
            let mut log = SessionLog::open(&pb, session_id).unwrap();
            log.append_exact_batch(&events, None, |pre_append_seq| {
                persist_cursor(
                    &cursor_path,
                    &CursorState::Committing {
                        version: CODEX_CURSOR_VERSION,
                        position: position.clone(),
                        pre_append_seq,
                        events: events.clone(),
                        events_sha256: hash_events(&events)?,
                    },
                )
            })
            .unwrap();
            let event_count = log.read_from(0).unwrap().len();

            let mut recovered = Tailer::new_durable(
                position.transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(recovered.pump().unwrap(), 0);
            assert_eq!(
                SessionLog::open(&pb, session_id)
                    .unwrap()
                    .read_from(0)
                    .unwrap()
                    .len(),
                event_count
            );
            let state = read_cursor(&cursor_path, session_id).unwrap().unwrap();
            assert!(matches!(state, CursorState::Committed { .. }));
            assert_eq!(state.position().codex_parser, position.codex_parser);
        });
    }

    #[test]
    fn durable_invalid_usage_does_not_advance_cursor_or_log() {
        crate::test_util::with_isolated_home("tailer-durable-invalid-usage", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-invalid-usage";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout.jsonl");
            std::fs::write(&transcript, format!("{}\n", codex_tokens(100, 50, 2))).unwrap();
            let mut tailer = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(tailer.pump().unwrap(), 1);
            let before = read_cursor(&cursor_path, session_id).unwrap().unwrap();
            let event_count = SessionLog::open(&pb, session_id)
                .unwrap()
                .read_from(0)
                .unwrap()
                .len();
            let invalid = serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {"total_token_usage": {
                        "input_tokens": 90,
                        "cached_input_tokens": 40,
                        "output_tokens": 1
                    }}
                }
            });
            append_lines(&transcript, &[invalid.to_string()]);
            let error = tailer
                .pump()
                .expect_err("decreasing cumulative usage must stop ingestion");
            assert!(format!("{error:#}").contains("decreased"));
            assert_eq!(
                read_cursor(&cursor_path, session_id)
                    .unwrap()
                    .unwrap()
                    .position(),
                before.position()
            );
            assert_eq!(
                SessionLog::open(&pb, session_id)
                    .unwrap()
                    .read_from(0)
                    .unwrap()
                    .len(),
                event_count
            );
        });
    }

    #[test]
    #[ignore = "reads the caller-provided private Codex rollout fixture"]
    fn replay_private_codex_rollout_fixture_offline() {
        let path = std::env::var_os("PILLBOX_CODEX_REPLAY_FIXTURE")
            .map(PathBuf::from)
            .expect("set PILLBOX_CODEX_REPLAY_FIXTURE to a local rollout.jsonl");
        crate::test_util::with_isolated_home("tailer-codex-private-replay", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-codex-private-replay";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let mut tailer = Tailer::new_durable(
                path.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert!(
                tailer.pump().unwrap() > 0,
                "private rollout should contain events"
            );
            drop(tailer);
            let mut replay = Tailer::new_durable(
                path,
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path,
            )
            .unwrap();
            assert_eq!(
                replay.pump().unwrap(),
                0,
                "committed private rollout must replay cleanly"
            );
            let events = SessionLog::open(&pb, session_id)
                .unwrap()
                .read_from(0)
                .unwrap();
            assert_burnin_projection(&events);
        });
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
    fn pump_preserves_utf8_split_across_read_chunks() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let empty = codex_complete("one", "");
        let content_marker = "\"last_agent_message\":\"";
        let content_start = empty.find(content_marker).unwrap() + content_marker.len();
        let padding = TRANSCRIPT_READ_CHUNK_BYTES - content_start - 1;
        let content = format!("{}🌶", "a".repeat(padding));
        let line = codex_complete("one", &content);
        assert_eq!(line.find('🌶').unwrap(), TRANSCRIPT_READ_CHUNK_BYTES - 1);
        let bytes = format!("{line}\n").into_bytes();

        let mut tailer = Tailer::new(path, "sess".into(), Harness::Codex, false, None);
        assert_eq!(
            tailer
                .ingest_bytes(&bytes[..TRANSCRIPT_READ_CHUNK_BYTES])
                .unwrap(),
            0,
            "first chunk ends inside UTF-8"
        );
        assert_eq!(
            tailer
                .ingest_bytes(&bytes[TRANSCRIPT_READ_CHUNK_BYTES..])
                .unwrap(),
            1,
            "next chunk completes the record"
        );
    }

    #[test]
    fn durable_sparse_unterminated_record_fails_before_attacker_length_is_read() {
        crate::test_util::with_isolated_home("tailer-durable-sparse-oversize", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-sparse";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout-sparse.jsonl");
            let file = std::fs::File::create(&transcript).unwrap();
            file.set_len(1024 * 1024 * 1024).unwrap();

            let mut tailer = Tailer::new_durable(
                transcript,
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            let error = loop {
                match tailer.pump() {
                    Ok(0) => {}
                    other => break other.expect_err("sparse record must cross the line bound"),
                }
            };

            assert!(error
                .to_string()
                .contains(&format!("exceeds {MAX_TRANSCRIPT_LINE_BYTES} bytes")));
            assert!(
                tailer.offset <= (MAX_TRANSCRIPT_LINE_BYTES + TRANSCRIPT_READ_CHUNK_BYTES) as u64
            );
            assert!(
                tailer.leftover.len() <= MAX_TRANSCRIPT_LINE_BYTES + TRANSCRIPT_READ_CHUNK_BYTES
            );
            assert_eq!(
                read_cursor(&cursor_path, session_id)
                    .unwrap()
                    .unwrap()
                    .position()
                    .offset,
                0,
                "rejected history must not advance the durable cursor"
            );
        });
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
    fn durable_cursor_replacement_does_not_replay_stale_codex_idle() {
        crate::test_util::with_isolated_home("tailer-durable-replacement", || {
            use crate::contract::Payload;

            let pb = crate::pillbox::global();
            let session_id = "sess-cursor";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout-test.jsonl");
            std::fs::write(&transcript, format!("{}\n", codex_complete("one", "first"))).unwrap();

            let mut first = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(first.pump().unwrap(), 1);
            drop(first);

            let mut replacement = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(replacement.pump().unwrap(), 0);
            {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&transcript)
                    .unwrap();
                writeln!(file, "{}", codex_complete("two", "second")).unwrap();
            }
            assert_eq!(replacement.pump().unwrap(), 1);

            let events = SessionLog::open(&pb, session_id)
                .unwrap()
                .read_from(0)
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event.payload, Payload::AttentionRequired(_)))
                    .count(),
                2,
                "one idle boundary per rollout turn, never a replay from byte zero"
            );
            let state = read_cursor(&cursor_path, session_id).unwrap().unwrap();
            assert_eq!(state.position().line_idx, 2);
            assert_eq!(
                state.position().offset,
                std::fs::metadata(transcript).unwrap().len()
            );
        });
    }

    #[test]
    fn durable_cursor_keeps_partial_line_bytes_across_replacement() {
        crate::test_util::with_isolated_home("tailer-durable-partial-line", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-partial";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout-test.jsonl");
            let line = codex_complete("one", "complete after restart");
            let split = line.len() / 2;
            std::fs::write(&transcript, &line.as_bytes()[..split]).unwrap();

            let mut first = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(first.pump().unwrap(), 0);
            assert_eq!(
                read_cursor(&cursor_path, session_id)
                    .unwrap()
                    .unwrap()
                    .position()
                    .offset,
                0
            );
            drop(first);

            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            file.write_all(&line.as_bytes()[split..]).unwrap();
            file.write_all(b"\n").unwrap();
            drop(file);
            let mut replacement = Tailer::new_durable(
                transcript,
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path,
            )
            .unwrap();
            assert_eq!(replacement.pump().unwrap(), 1);
        });
    }

    #[test]
    fn durable_cursor_does_not_reread_a_retained_partial_line() {
        crate::test_util::with_isolated_home("tailer-durable-live-partial", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-live-partial";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout-test.jsonl");
            let first = codex_complete("one", "first");
            let second = codex_complete("two", "second");
            let split = second.len() / 2;
            std::fs::write(&transcript, format!("{first}\n{}", &second[..split])).unwrap();

            let mut tailer = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(tailer.pump().unwrap(), 1);
            let read_offset = std::fs::metadata(&transcript).unwrap().len();
            assert_eq!(tailer.offset, read_offset);
            assert_eq!(
                read_cursor(&cursor_path, session_id)
                    .unwrap()
                    .unwrap()
                    .position()
                    .offset,
                first.len() as u64 + 1
            );

            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            file.write_all(&second.as_bytes()[split..]).unwrap();
            file.write_all(b"\n").unwrap();
            drop(file);
            assert_eq!(tailer.pump().unwrap(), 1);
            assert_eq!(
                read_cursor(&cursor_path, session_id)
                    .unwrap()
                    .unwrap()
                    .position()
                    .offset,
                std::fs::metadata(transcript).unwrap().len()
            );
        });
    }

    #[test]
    fn durable_cursor_tracks_lines_across_multiple_unaligned_read_chunks() {
        crate::test_util::with_isolated_home("tailer-durable-multichunk", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-multichunk";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout-test.jsonl");
            let content = "x".repeat(40 * 1024);
            let body = (0..4)
                .map(|index| codex_complete(&index.to_string(), &content))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            assert!(body.len() > 2 * TRANSCRIPT_READ_CHUNK_BYTES);
            assert_ne!(
                body.lines().next().unwrap().len() % TRANSCRIPT_READ_CHUNK_BYTES,
                0
            );
            std::fs::write(&transcript, &body).unwrap();

            let mut tailer = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(tailer.pump().unwrap(), 4);
            assert_eq!(tailer.offset, body.len() as u64);
            let cursor = read_cursor(&cursor_path, session_id).unwrap().unwrap();
            assert_eq!(cursor.position().offset, body.len() as u64);
            assert_eq!(cursor.position().line_idx, 4);
        });
    }

    #[test]
    fn durable_cursor_rejects_same_path_with_changed_file_identity() {
        crate::test_util::with_isolated_home("tailer-durable-replaced-rollout", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-replaced-rollout";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout-test.jsonl");
            let first = codex_complete("one", "first");
            std::fs::write(&transcript, format!("{first}\n")).unwrap();

            let mut tailer = Tailer::new_durable(
                transcript.clone(),
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(tailer.pump().unwrap(), 1);
            let committed = read_cursor(&cursor_path, session_id).unwrap().unwrap();

            // Keep the old inode alive under another name so the new file at the
            // rollout path cannot reuse it. A longer replacement proves identity
            // is checked before the length/offset fast paths.
            std::fs::rename(&transcript, session_dir.join("old-rollout.jsonl")).unwrap();
            let replacement = format!(
                "{}\n{}\n",
                codex_complete("two", "replacement"),
                codex_complete("three", "longer replacement")
            );
            assert!(replacement.len() as u64 >= committed.position().offset);
            std::fs::write(&transcript, replacement).unwrap();

            let error = tailer
                .pump()
                .expect_err("a replaced rollout must not inherit the prior cursor");
            assert!(error.to_string().contains("changed file identity"));
            assert_eq!(
                read_cursor(&cursor_path, session_id)
                    .unwrap()
                    .unwrap()
                    .position(),
                committed.position(),
                "rejection must not advance the durable cursor"
            );
        });
    }

    #[test]
    fn durable_cursor_recovers_append_before_cursor_commit_without_duplication() {
        crate::test_util::with_isolated_home("tailer-durable-append-recovery", || {
            let pb = crate::pillbox::global();
            let session_id = "sess-recover";
            let session_dir = crate::session::session_dir(&pb, session_id).unwrap();
            let cursor_path = session_dir.join(".tailer-cursor.json");
            let transcript = session_dir.join("rollout-test.jsonl");
            let line = codex_complete("one", "recover me");
            std::fs::write(&transcript, format!("{line}\n")).unwrap();
            let (device, inode) = source_identity(&transcript).unwrap();
            let parsed = parse_with(Harness::Codex, &line, 0);
            let events: Vec<_> = parsed
                .iter()
                .flat_map(contract_map::to_payloads)
                .map(|payload| {
                    Event::session(session_id, payload)
                        .with_actor(Actor::agent(Harness::Codex.agent_id()))
                })
                .collect();
            let position = CursorPosition {
                transcript: transcript.clone(),
                device,
                inode,
                offset: line.len() as u64 + 1,
                line_idx: 1,
                codex_parser: Some(codex::Parser::default().state()),
            };
            let mut log = SessionLog::open(&pb, session_id).unwrap();
            log.append_exact_batch(&events, None, |pre_append_seq| {
                assert_eq!(pre_append_seq, 0);
                persist_cursor(
                    &cursor_path,
                    &CursorState::Committing {
                        version: CODEX_CURSOR_VERSION,
                        position: position.clone(),
                        pre_append_seq,
                        events: events.clone(),
                        events_sha256: hash_events(&events)?,
                    },
                )
            })
            .unwrap();
            let before = log.read_from(0).unwrap().len();

            let mut replacement = Tailer::new_durable(
                transcript,
                session_id.into(),
                Harness::Codex,
                true,
                SessionLog::open(&pb, session_id).unwrap(),
                cursor_path.clone(),
            )
            .unwrap();
            assert_eq!(replacement.pump().unwrap(), 0);
            assert_eq!(
                SessionLog::open(&pb, session_id)
                    .unwrap()
                    .read_from(0)
                    .unwrap()
                    .len(),
                before
            );
            assert!(matches!(
                read_cursor(&cursor_path, session_id).unwrap(),
                Some(CursorState::Committed { .. })
            ));
        });
    }

    #[test]
    fn record_length_admission_accepts_the_limit_and_rejects_one_more_byte() {
        let path = Path::new("rollout.jsonl");
        assert!(validate_record_lengths(&vec![b'x'; MAX_TRANSCRIPT_LINE_BYTES], path).is_ok());
        let error = validate_record_lengths(&vec![b'x'; MAX_TRANSCRIPT_LINE_BYTES + 1], path)
            .expect_err("one byte over the record limit must fail");
        assert!(error.to_string().contains("rollout.jsonl"));
    }
}
