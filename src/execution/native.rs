//! One bounded native Codex turn. The caller owns VM teardown and result capture.
//! Evidence contains actual directional RPC frames, never model-authored lifecycle claims.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::{json, Value};

use super::files::{FileBroker, FileOperation, MAX_FILE_BYTES, MAX_TOOL_CALLS};
use super::protocol::{self, CodexProfile, FileCall, TurnTerminal};

const MAX_FRAME_BYTES: usize = 12 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FRAMES: usize = 16_384;
const MAX_PENDING_FRAMES: usize = 256;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeLimits {
    pub(crate) deadline: Instant,
    /// JSON bytes, excluding the terminating LF, in either direction.
    pub(crate) max_frame_bytes: usize,
    /// All bytes received from the native server, including framing.
    pub(crate) max_input_bytes: u64,
    /// All bytes sent to the native server, including framing.
    pub(crate) max_output_bytes: u64,
    /// Serialized directional evidence envelopes, including one LF per frame.
    pub(crate) max_evidence_bytes: u64,
    pub(crate) max_tool_calls: u64,
    pub(crate) max_write_bytes: u64,
}

impl NativeLimits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_FRAME_BYTES).contains(&self.max_frame_bytes),
            "invalid native frame byte limit"
        );
        for limit in [
            self.max_input_bytes,
            self.max_output_bytes,
            self.max_evidence_bytes,
        ] {
            ensure!(
                (1..=MAX_TOTAL_BYTES).contains(&limit),
                "invalid native total byte limit"
            );
        }
        ensure!(
            (1..=MAX_TOOL_CALLS).contains(&self.max_tool_calls),
            "invalid native tool-call limit"
        );
        ensure!(
            (1..=MAX_FILE_BYTES).contains(&self.max_write_bytes),
            "invalid native write byte limit"
        );
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NativeResult {
    pub(crate) text: String,
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) evidence: Vec<Value>,
}

pub(crate) struct NativeFailure {
    pub(crate) error: anyhow::Error,
    pub(crate) evidence: Vec<Value>,
}

impl fmt::Debug for NativeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // File contents and model text belong only in explicitly persisted evidence.
        f.debug_struct("NativeFailure")
            .field("error", &self.error.to_string())
            .finish()
    }
}

impl fmt::Display for NativeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for NativeFailure {}

pub(crate) fn run(
    stream: UnixStream,
    profile: &CodexProfile,
    rendered_input: &str,
    operations: &[FileOperation],
    broker: &mut FileBroker,
    limits: NativeLimits,
    poll: impl FnMut() -> Result<()>,
) -> std::result::Result<NativeResult, NativeFailure> {
    let mut wire = Wire::new(stream, limits, poll).map_err(|error| NativeFailure {
        error,
        evidence: Vec::new(),
    })?;
    let outcome = (|| {
        limits.validate()?;
        wire.check()?;
        // Validate static input before the first native request can have effects.
        let thread_params = profile.thread_start(operations)?;
        profile.turn_start("validation", rendered_input)?;
        let mut pending = VecDeque::new();
        let mut server_ids = HashSet::new();
        let initialized = request(
            &mut wire,
            "pillbox-init",
            "initialize",
            protocol::initialize_params(),
            &mut pending,
            &mut server_ids,
        )?;
        ensure!(initialized.is_object(), "invalid native initialize result");
        wire.send(json!({"method":"initialized","params":{}}))?;
        let thread = request(
            &mut wire,
            "pillbox-thread",
            "thread/start",
            thread_params,
            &mut pending,
            &mut server_ids,
        )?;
        let thread_id = profile.validate_thread_start(&thread)?;
        let turn = request(
            &mut wire,
            "pillbox-turn",
            "turn/start",
            profile.turn_start(&thread_id, rendered_input)?,
            &mut pending,
            &mut server_ids,
        )?;
        let turn_id = protocol::validate_turn_started(&turn)?;
        let mut calls = HashSet::new();
        let mut text = String::new();
        loop {
            wire.check()?;
            let frame = if let Some(index) = pending.pop_front() {
                wire.evidence[index]["message"].clone()
            } else {
                let (frame, _) = wire.receive()?;
                observe_server_id(&frame, &mut server_ids)?;
                frame
            };
            let method = frame["method"]
                .as_str()
                .context("unexpected native RPC response")?;
            let params = &frame["params"];
            if let Some(id) = frame.get("id") {
                if method != "item/tool/call" {
                    wire.send(protocol::unsupported_server_request(id)?)?;
                    bail!("unsupported native server request");
                }
                dispatch(
                    &mut wire, id, params, &thread_id, &turn_id, operations, broker, &mut calls,
                )?;
                continue;
            }
            correlate_notification(params, &thread_id, &turn_id)?;
            match method {
                "thread/started" => ensure!(
                    params["thread"]["id"] == thread_id,
                    "uncorrelated native thread notification"
                ),
                "turn/started" => ensure!(
                    params["threadId"] == thread_id
                        && protocol::validate_turn_started(params)? == turn_id,
                    "uncorrelated native turn notification"
                ),
                "item/completed" => {
                    ensure!(
                        params["threadId"] == thread_id && params["turnId"] == turn_id,
                        "uncorrelated native item notification"
                    );
                    harvest_text(&params["item"], &mut text)?;
                }
                "model/rerouted" => bail!("native model rerouting is unsupported"),
                "turn/completed" => {
                    match protocol::validate_turn_terminal(params, &thread_id, &turn_id)? {
                        TurnTerminal::Completed => {}
                        TurnTerminal::Failed => bail!("native turn failed"),
                        TurnTerminal::Interrupted => bail!("native turn interrupted"),
                    }
                    // Native terminal items can carry final text even without item notifications.
                    for item in params["turn"]["items"].as_array().unwrap() {
                        harvest_text(item, &mut text)?;
                    }
                    ensure!(
                        pending.is_empty(),
                        "native events queued after terminal completion"
                    );
                    wire.check()?;
                    return Ok((text, thread_id, turn_id));
                }
                // Unrecognized notifications remain evidence, never lifecycle authority.
                _ => {}
            }
        }
    })();
    match outcome {
        Ok((text, thread_id, turn_id)) => Ok(NativeResult {
            text,
            thread_id,
            turn_id,
            evidence: wire.evidence,
        }),
        Err(error) => Err(NativeFailure {
            error,
            evidence: wire.evidence,
        }),
    }
}

fn request<P: FnMut() -> Result<()>>(
    wire: &mut Wire<P>,
    id: &str,
    method: &str,
    params: Value,
    pending: &mut VecDeque<usize>,
    server_ids: &mut HashSet<String>,
) -> Result<Value> {
    wire.send(json!({"id":id,"method":method,"params":params}))?;
    loop {
        let (frame, index) = wire.receive()?;
        if frame.get("method").is_some() {
            observe_server_id(&frame, server_ids)?;
            if let Some(id) = frame.get("id") {
                if frame["method"] != "item/tool/call" {
                    wire.send(protocol::unsupported_server_request(id)?)?;
                    bail!("unsupported native server request");
                }
            }
            ensure!(
                pending.len() < MAX_PENDING_FRAMES,
                "native pending-frame limit exceeded"
            );
            pending.push_back(index);
        } else {
            ensure!(
                frame["id"] == id,
                "uncorrelated or duplicate native RPC response"
            );
            ensure!(frame.get("error").is_none(), "native RPC request failed");
            return frame
                .get("result")
                .cloned()
                .context("missing native RPC result");
        }
    }
}

fn observe_server_id(frame: &Value, seen: &mut HashSet<String>) -> Result<()> {
    if frame.get("method").is_some() {
        if let Some(id) = frame.get("id") {
            protocol::unsupported_server_request(id)?;
            ensure!(
                seen.insert(id.to_string()),
                "duplicate native server request id"
            );
        }
    }
    Ok(())
}

fn correlate_notification(params: &Value, thread_id: &str, turn_id: &str) -> Result<()> {
    for (key, expected) in [("threadId", thread_id), ("turnId", turn_id)] {
        if let Some(actual) = params.get(key) {
            ensure!(actual == expected, "uncorrelated native notification");
        }
    }
    Ok(())
}

fn harvest_text(item: &Value, text: &mut String) -> Result<()> {
    if item["type"] == "agentMessage" && item["phase"] == "final_answer" {
        *text = item["text"]
            .as_str()
            .context("invalid native final assistant text")?
            .to_owned();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dispatch<P: FnMut() -> Result<()>>(
    wire: &mut Wire<P>,
    id: &Value,
    params: &Value,
    thread_id: &str,
    turn_id: &str,
    operations: &[FileOperation],
    broker: &mut FileBroker,
    calls: &mut HashSet<String>,
) -> Result<()> {
    let parsed = protocol::parse_dynamic_call(
        params,
        thread_id,
        turn_id,
        operations,
        wire.limits.max_write_bytes,
    );
    let call = match parsed {
        Ok(call) => call,
        Err(error) => {
            wire.send(protocol::unsupported_server_request(id)?)?;
            // Serde's inner errors can quote invalid argument values. Keep only
            // the parser's bounded, contextual cause outside raw evidence.
            bail!("native file request rejected: {error}");
        }
    };
    ensure!(
        calls.len() < wire.limits.max_tool_calls as usize,
        "native tool-call limit exhausted"
    );
    ensure!(calls.insert(call.call_id), "duplicate native tool call id");
    let denied = wire.prepare(tool_response(id, false, "File operation rejected".into()))?;
    // Reserve the encoded acknowledgment and its evidence before any mutation.
    let accepted = wire.prepare(tool_response(id, true, "{\"ok\":true}".into()))?;
    wire.check()?;
    let operation = match call.operation {
        FileCall::Read { path } => broker.read(&path).and_then(|entry| {
            let limit = wire
                .limits
                .max_frame_bytes
                .min((wire.limits.max_output_bytes - wire.output_bytes).saturating_sub(1) as usize);
            file_contents(&entry, limit).map(Some)
        }),
        FileCall::Write {
            path,
            executable,
            bytes,
        } => broker.write(&path, executable, &bytes).map(|()| None),
        FileCall::Remove { path } => broker.remove(&path).map(|()| None),
    };
    match operation {
        Ok(Some(contents)) => wire.send(tool_response(id, true, contents)),
        Ok(None) => wire.send_prepared(accepted),
        Err(error) => {
            wire.send_prepared(denied)?;
            Err(error.context("native file operation rejected"))
        }
    }
}

fn file_contents(entry: &super::files::FileEntry, limit: usize) -> Result<String> {
    let (encoding, content) = match std::str::from_utf8(&entry.bytes) {
        Ok(text) => ("utf8", std::borrow::Cow::Borrowed(text)),
        Err(_) => {
            ensure!(
                entry.bytes.len().div_ceil(3) * 4 <= limit,
                "encoded read exceeds native frame limit"
            );
            (
                "base64",
                std::borrow::Cow::Owned(BASE64.encode(&entry.bytes)),
            )
        }
    };
    #[derive(serde::Serialize)]
    struct Contents<'a> {
        path: &'a str,
        executable: bool,
        encoding: &'a str,
        content: &'a str,
    }
    // Text escaping is bounded here; the outer RPC/evidence encoding is bounded
    // again by Wire::prepare before any bytes are sent.
    let mut writer = BoundedBytes {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(
        &mut writer,
        &Contents {
            path: &entry.path,
            executable: entry.executable,
            encoding,
            content: &content,
        },
    )
    .context("file contents exceed native frame limit")?;
    String::from_utf8(writer.bytes).context("invalid encoded file response")
}

fn tool_response(id: &Value, success: bool, text: String) -> Value {
    json!({"id":id,"result":{"contentItems":[{"type":"inputText","text":text}],"success":success}})
}

struct Prepared {
    bytes: Vec<u8>,
    envelope: Value,
    evidence_bytes: u64,
}

struct Wire<P> {
    stream: UnixStream,
    limits: NativeLimits,
    poll: P,
    chunk: [u8; 8192],
    start: usize,
    end: usize,
    input_bytes: u64,
    output_bytes: u64,
    evidence_bytes: u64,
    evidence: Vec<Value>,
}

impl<P: FnMut() -> Result<()>> Wire<P> {
    fn new(stream: UnixStream, limits: NativeLimits, poll: P) -> Result<Self> {
        stream
            .set_nonblocking(true)
            .context("configure native stream nonblocking mode")?;
        Ok(Self {
            stream,
            limits,
            poll,
            chunk: [0; 8192],
            start: 0,
            end: 0,
            input_bytes: 0,
            output_bytes: 0,
            evidence_bytes: 0,
            evidence: Vec::new(),
        })
    }

    fn check(&mut self) -> Result<Duration> {
        (self.poll)().context("native invocation cancelled or no longer owned")?;
        let remaining = self
            .limits
            .deadline
            .checked_duration_since(Instant::now())
            .context("native execution deadline exceeded")?;
        ensure!(!remaining.is_zero(), "native execution deadline exceeded");
        Ok(remaining.min(POLL_INTERVAL))
    }

    fn wait_ready(&mut self, events: libc::c_short) -> Result<()> {
        loop {
            let remaining = self.check()?;
            let mut descriptor = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events,
                revents: 0,
            };
            // Poll retains the current deadline and can drain data after HUP.
            // macOS rejects SO_RCVTIMEO updates on a disconnected Unix socket.
            let ready =
                unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis() as libc::c_int) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("poll native stream");
            }
            if ready > 0 {
                ensure!(
                    descriptor.revents & libc::POLLNVAL == 0,
                    "invalid native stream descriptor"
                );
                self.check()?;
                return Ok(());
            }
        }
    }

    fn receive(&mut self) -> Result<(Value, usize)> {
        let mut line = Vec::new();
        loop {
            self.check()?;
            if self.start == self.end {
                self.wait_ready(libc::POLLIN)?;
                let available = (self.limits.max_input_bytes - self.input_bytes + 1)
                    .min(self.chunk.len() as u64) as usize;
                let count = match self.stream.read(&mut self.chunk[..available]) {
                    Ok(0) => bail!("native stream closed before terminal completion"),
                    Ok(count) => count,
                    Err(error) if transient(&error) => continue,
                    Err(error) => return Err(error).context("read native stream"),
                };
                ensure!(
                    count as u64 <= self.limits.max_input_bytes - self.input_bytes,
                    "native input byte limit exceeded"
                );
                self.input_bytes += count as u64;
                self.start = 0;
                self.end = count;
            }
            let available = &self.chunk[self.start..self.end];
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.unwrap_or(available.len());
            ensure!(
                count <= self.limits.max_frame_bytes - line.len(),
                "native frame byte limit exceeded"
            );
            extend_within_limit(&mut line, &available[..count], self.limits.max_frame_bytes);
            self.start += count;
            if newline.is_some() {
                self.start += 1;
                ensure!(!line.is_empty(), "empty native frame");
                let frame: Value =
                    serde_json::from_slice(&line).context("invalid native JSON frame")?;
                let envelope = json!({"direction":"inbound","message":frame});
                let evidence_bytes = self.evidence_size(&envelope)?;
                let index = self.evidence.len();
                self.evidence.push(envelope);
                self.evidence_bytes += evidence_bytes;
                validate_envelope(&frame)?;
                return Ok((frame, index));
            }
        }
    }

    fn evidence_size(&self, envelope: &Value) -> Result<u64> {
        ensure!(
            self.evidence.len() < MAX_FRAMES,
            "native evidence frame limit exceeded"
        );
        let mut counter = ByteCounter {
            bytes: 1,
            limit: self.limits.max_evidence_bytes - self.evidence_bytes,
        };
        serde_json::to_writer(&mut counter, envelope)
            .context("native evidence byte limit exceeded")?;
        Ok(counter.bytes)
    }

    fn prepare(&self, frame: Value) -> Result<Prepared> {
        let remaining = self.limits.max_output_bytes - self.output_bytes;
        ensure!(remaining > 0, "native output byte limit exceeded");
        let mut writer = BoundedBytes {
            bytes: Vec::new(),
            limit: self.limits.max_frame_bytes.min((remaining - 1) as usize),
        };
        serde_json::to_writer(&mut writer, &frame)
            .context("native output frame or byte limit exceeded")?;
        writer.bytes.reserve_exact(1);
        writer.bytes.push(b'\n');
        let envelope = json!({"direction":"outbound","message":frame});
        let evidence_bytes = self.evidence_size(&envelope)?;
        Ok(Prepared {
            bytes: writer.bytes,
            envelope,
            evidence_bytes,
        })
    }

    fn send(&mut self, frame: Value) -> Result<()> {
        let prepared = self.prepare(frame)?;
        self.send_prepared(prepared)
    }

    fn send_prepared(&mut self, prepared: Prepared) -> Result<()> {
        let mut remaining = prepared.bytes.as_slice();
        while !remaining.is_empty() {
            self.wait_ready(libc::POLLOUT)?;
            match self.stream.write(remaining) {
                Ok(0) => bail!("native stream stopped accepting output"),
                Ok(count) => {
                    self.output_bytes += count as u64;
                    remaining = &remaining[count..];
                }
                Err(error) if transient(&error) => continue,
                Err(error) => return Err(error).context("write native stream"),
            }
        }
        self.evidence_bytes += prepared.evidence_bytes;
        self.evidence.push(prepared.envelope);
        Ok(())
    }
}

fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn validate_envelope(frame: &Value) -> Result<()> {
    let object = frame
        .as_object()
        .context("native RPC frame is not an object")?;
    ensure!(
        object.keys().all(|key| matches!(
            key.as_str(),
            "jsonrpc" | "id" | "method" | "params" | "result" | "error" | "emittedAtMs"
        )),
        "unknown native RPC envelope field"
    );
    ensure!(
        frame.get("jsonrpc").is_none_or(|version| version == "2.0"),
        "invalid native RPC version"
    );
    // Codex 0.151.0 ServerNotificationEnvelope adds an optional int64 timestamp.
    // Preserve it as evidence only; requests/responses and lifecycle authority are unchanged.
    if let Some(timestamp) = frame.get("emittedAtMs") {
        ensure!(
            frame.get("method").is_some()
                && frame.get("id").is_none()
                && timestamp.as_i64().is_some(),
            "invalid native notification emission timestamp"
        );
    }
    if let Some(method) = frame.get("method") {
        ensure!(
            method.as_str().is_some_and(|method| !method.is_empty()),
            "invalid native RPC method"
        );
        ensure!(
            frame.get("result").is_none() && frame.get("error").is_none(),
            "ambiguous native RPC frame"
        );
        ensure!(
            frame.get("params").is_none_or(Value::is_object),
            "invalid native RPC params"
        );
    } else {
        ensure!(
            frame.get("id").is_some() && frame.get("params").is_none(),
            "invalid native RPC response"
        );
        ensure!(
            frame.get("result").is_some() != frame.get("error").is_some(),
            "ambiguous native RPC response"
        );
    }
    Ok(())
}

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::other("byte limit exceeded"));
        }
        extend_within_limit(&mut self.bytes, bytes, self.limit);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn extend_within_limit(buffer: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    let required = buffer.len() + bytes.len();
    if required > buffer.capacity() {
        // Vec's implicit growth can otherwise allocate beyond the admitted cap.
        let capacity = required.max(buffer.capacity().saturating_mul(2)).min(limit);
        buffer.reserve_exact(capacity - buffer.len());
    }
    buffer.extend_from_slice(bytes);
}

struct ByteCounter {
    bytes: u64,
    limit: u64,
}

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes > self.limit || bytes.len() as u64 > self.limit - self.bytes {
            return Err(io::Error::other("byte limit exceeded"));
        }
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;

    use super::super::files::{FileEntry, FileLimits, FilePolicy, FileTree};

    const OPERATIONS: &[FileOperation] = &[
        FileOperation::Read,
        FileOperation::Remove,
        FileOperation::Write,
    ];
    const INPUT: &str = "exact\ninput";

    fn profile() -> CodexProfile {
        CodexProfile::new("gpt-5.6-sol", "high").unwrap()
    }

    fn limits() -> NativeLimits {
        NativeLimits {
            deadline: Instant::now() + Duration::from_secs(3),
            max_frame_bytes: 1024 * 1024,
            max_input_bytes: 4 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_evidence_bytes: 8 * 1024 * 1024,
            max_tool_calls: 10,
            max_write_bytes: 1024,
        }
    }

    fn broker() -> FileBroker {
        let limits = FileLimits {
            max_file_bytes: 1024,
            max_snapshot_bytes: 4096,
            max_tool_calls: 10,
            max_output_bytes: 4096,
        };
        let tree = FileTree::new(
            vec![FileEntry {
                path: "a".into(),
                executable: false,
                bytes: vec![0, 255, 1],
            }],
            &limits,
        )
        .unwrap();
        let policy = FilePolicy::new(
            vec!["a".into()],
            vec!["a".into()],
            OPERATIONS.to_vec(),
            limits,
        )
        .unwrap();
        FileBroker::new(tree, policy).unwrap()
    }

    fn thread_result() -> Value {
        json!({
            "model":"gpt-5.6-sol", "modelProvider":protocol::PROVIDER_ID,
            "reasoningEffort":"high", "approvalPolicy":"never", "approvalsReviewer":"user",
            "cwd":protocol::CODEX_CWD,"runtimeWorkspaceRoots":[],"instructionSources":[],
            "sandbox":{"type":"readOnly","networkAccess":false},
            "thread":{"id":"thread-1","cliVersion":protocol::CODEX_VERSION,
                "modelProvider":protocol::PROVIDER_ID,"cwd":protocol::CODEX_CWD,"ephemeral":true}
        })
    }

    fn turn_result() -> Value {
        json!({"turn":{"id":"turn-1","status":"inProgress","items":[],"error":null}})
    }

    fn terminal() -> Value {
        json!({"method":"turn/completed","params":{"threadId":"thread-1",
            "turn":{"id":"turn-1","status":"completed","items":[],"error":null}}})
    }

    fn tool(id: i64, call_id: &str, name: &str, arguments: Value) -> Value {
        json!({"id":id,"method":"item/tool/call","params":{
            "threadId":"thread-1","turnId":"turn-1","callId":call_id,
            "namespace":null,"tool":name,"arguments":arguments,
        }})
    }

    fn write_call(id: i64, call_id: &str, bytes: &[u8]) -> Value {
        tool(
            id,
            call_id,
            "pillbox_write_file",
            json!({"path":"a","executable":true,"encoding":"base64","content":BASE64.encode(bytes)}),
        )
    }

    struct Peer {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl Peer {
        fn new(stream: UnixStream) -> Self {
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            Self {
                reader: BufReader::new(stream.try_clone().unwrap()),
                writer: stream,
            }
        }
        fn read(&mut self) -> Value {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap() > 0,
                "unexpected native client EOF"
            );
            serde_json::from_str(&line).unwrap()
        }
        fn send(&mut self, frame: Value) {
            serde_json::to_writer(&mut self.writer, &frame).unwrap();
            self.writer.write_all(b"\n").unwrap();
        }
        fn begin(&mut self) {
            assert_eq!(
                self.read(),
                json!({"id":"pillbox-init","method":"initialize","params":protocol::initialize_params()})
            );
            self.send(json!({"id":"pillbox-init","result":{"userAgent":"codex/0.151.0"}}));
            assert_eq!(self.read(), json!({"method":"initialized","params":{}}));
            assert_eq!(
                self.read(),
                json!({"id":"pillbox-thread","method":"thread/start","params":profile().thread_start(OPERATIONS).unwrap()})
            );
            self.send(json!({"id":"pillbox-thread","result":thread_result()}));
            assert_eq!(
                self.read(),
                json!({"id":"pillbox-turn","method":"turn/start","params":profile().turn_start("thread-1", INPUT).unwrap()})
            );
        }
        fn started(&mut self) {
            self.begin();
            self.send(json!({"id":"pillbox-turn","result":turn_result()}));
        }
    }

    fn drive(
        broker: &mut FileBroker,
        limits: NativeLimits,
        script: impl FnOnce(&mut Peer) + Send + 'static,
    ) -> std::result::Result<NativeResult, NativeFailure> {
        let (client, server) = UnixStream::pair().unwrap();
        let peer = thread::spawn(move || script(&mut Peer::new(server)));
        let result = run(
            client,
            &profile(),
            INPUT,
            OPERATIONS,
            broker,
            limits,
            || Ok(()),
        );
        peer.join().unwrap();
        result
    }

    #[test]
    fn text_write_and_read_round_trip_exact_utf8_without_base64() {
        let mut broker = broker();
        let text = "fn main() { println!(\"→\\n\"); }\n";
        drive(&mut broker, limits(), move |peer| {
            peer.started();
            peer.send(tool(
                10,
                "write-text",
                "pillbox_write_file",
                json!({
                    "path":"a", "executable":false, "encoding":"utf8", "content":text,
                }),
            ));
            assert_eq!(peer.read()["result"]["success"], true);
            peer.send(tool(
                11,
                "read-text",
                "pillbox_read_file",
                json!({"path":"a"}),
            ));
            let response = peer.read();
            let contents: Value = serde_json::from_str(
                response["result"]["contentItems"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                contents,
                json!({
                    "path":"a", "executable":false, "encoding":"utf8", "content":text,
                })
            );
            peer.send(terminal());
        })
        .unwrap();
        assert_eq!(
            broker.finish().unwrap().tree.entries()[0].bytes,
            text.as_bytes()
        );
    }

    #[test]
    fn text_byte_and_wire_limits_reject_before_mutation() {
        for kind in ["bytes", "frame", "codec"] {
            let mut broker = broker();
            let mut limits = limits();
            let content = match kind {
                "bytes" => "→".repeat(342),
                "frame" => {
                    limits.max_frame_bytes = serde_json::to_vec(&json!({
                        "id":"pillbox-thread", "method":"thread/start",
                        "params":profile().thread_start(OPERATIONS).unwrap(),
                    }))
                    .unwrap()
                    .len();
                    "\0".repeat(1024)
                }
                _ => "valid text".into(),
            };
            let failure = drive(&mut broker, limits, move |peer| {
                peer.started();
                let call = tool(
                    10,
                    "write",
                    "pillbox_write_file",
                    json!({
                        "path":"a", "executable":false,
                        "encoding":if kind == "codec" { "hex" } else { "utf8" },
                        "content":content,
                    }),
                );
                let mut bytes = serde_json::to_vec(&call).unwrap();
                bytes.push(b'\n');
                peer.writer.write_all(&bytes).unwrap();
                if kind != "frame" {
                    assert_eq!(peer.read()["error"]["code"], -32601);
                }
            })
            .unwrap_err();
            assert!(
                failure.to_string().contains(if kind == "frame" {
                    "frame byte limit"
                } else {
                    "native file request rejected"
                }),
                "{kind}: {failure}"
            );
            assert_eq!(broker.usage().tool_calls, 0);
            assert!(broker.finish().unwrap().changed_paths.is_empty());
        }
    }

    #[test]
    fn text_read_escaping_is_bounded_at_both_json_layers() {
        let entry = FileEntry {
            path: "a".into(),
            executable: false,
            bytes: vec![0; 64],
        };
        assert!(file_contents(&entry, 128).is_err());
        let contents = file_contents(&entry, 1024).unwrap();
        let (client, _server) = UnixStream::pair().unwrap();
        let mut limits = limits();
        limits.max_frame_bytes = contents.len();
        let wire = Wire::new(client, limits, || Ok(())).unwrap();
        assert!(wire
            .prepare(tool_response(&json!(1), true, contents))
            .is_err());
    }

    #[test]
    fn exact_handshake_one_turn_and_typed_file_operations_preserve_evidence() {
        let mut broker = broker();
        let result = drive(&mut broker, limits(), |peer| {
            peer.started();
            peer.send(tool(40,"read-1","pillbox_read_file",json!({"path":"a"})));
            let response = peer.read();
            assert_eq!(response["id"], 40);
            assert_eq!(response["result"]["success"], true);
            let contents: Value = serde_json::from_str(response["result"]["contentItems"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(contents,json!({"path":"a","encoding":"base64","content":"AP8B","executable":false}));
            peer.send(write_call(41,"write-1",b"replacement"));
            assert_eq!(peer.read(),tool_response(&json!(41),true,"{\"ok\":true}".into()));
            peer.send(tool(42,"remove-1","pillbox_remove_file",json!({"path":"a"})));
            assert_eq!(peer.read(),tool_response(&json!(42),true,"{\"ok\":true}".into()));
            peer.send(json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1",
                "item":{"type":"agentMessage","id":"m-1","phase":"final_answer","text":"first"}}}));
            peer.send(json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1",
                "item":{"type":"agentMessage","id":"m-2","phase":"commentary","text":"not final"}}}));
            let mut done = terminal();
            done["params"]["turn"]["items"] = json!([{"type":"agentMessage","id":"m-3","phase":"final_answer","text":"last final"}]);
            peer.send(done);
        }).unwrap();
        assert_eq!(result.text, "last final");
        assert_eq!(result.thread_id, "thread-1");
        assert_eq!(result.turn_id, "turn-1");
        assert_eq!(broker.usage().tool_calls, 3);
        assert!(broker.finish().unwrap().tree.entries().is_empty());
        let starts = result
            .evidence
            .iter()
            .filter(|frame| {
                frame["direction"] == "outbound" && frame["message"]["method"] == "turn/start"
            })
            .count();
        assert_eq!(starts, 1);
        assert_eq!(
            result.evidence.last().unwrap()["message"]["method"],
            "turn/completed"
        );
    }

    #[test]
    fn queues_early_tool_calls_until_turn_id_is_confirmed() {
        let mut broker = broker();
        drive(&mut broker, limits(), |peer| {
            peer.begin();
            peer.send(write_call(10, "early", b"new"));
            peer.reader
                .get_mut()
                .set_read_timeout(Some(Duration::from_millis(30)))
                .unwrap();
            let mut byte = [0];
            assert!(transient(
                &peer.reader.get_mut().read(&mut byte).unwrap_err()
            ));
            peer.reader
                .get_mut()
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            peer.send(json!({"id":"pillbox-turn","result":turn_result()}));
            assert_eq!(peer.read()["result"]["success"], true);
            peer.send(terminal());
        })
        .unwrap();
        assert_eq!(broker.finish().unwrap().tree.entries()[0].bytes, b"new");
    }

    #[test]
    fn mismatched_early_turn_never_mutates_the_broker() {
        let mut broker = broker();
        let failure = drive(&mut broker, limits(), |peer| {
            peer.begin();
            peer.send(write_call(10, "early", b"new"));
            let mut result = turn_result();
            result["turn"]["id"] = json!("different-turn");
            peer.send(json!({"id":"pillbox-turn","result":result}));
            assert_eq!(peer.read()["error"]["code"], -32601);
        })
        .unwrap_err();
        assert!(failure.to_string().contains("rejected"));
        assert_eq!(broker.usage().tool_calls, 0);
        assert!(broker.finish().unwrap().changed_paths.is_empty());
    }

    #[test]
    fn duplicate_tool_or_rpc_id_is_never_executed_twice() {
        for duplicate_rpc in [false, true] {
            let mut broker = broker();
            let failure = drive(&mut broker, limits(), move |peer| {
                peer.started();
                peer.send(write_call(10, "call-1", b"first"));
                assert_eq!(peer.read()["result"]["success"], true);
                peer.send(write_call(
                    if duplicate_rpc { 10 } else { 11 },
                    if duplicate_rpc { "call-2" } else { "call-1" },
                    b"second",
                ));
            })
            .unwrap_err();
            assert!(failure.to_string().contains("duplicate"));
            assert_eq!(broker.usage().tool_calls, 1);
            assert_eq!(broker.finish().unwrap().tree.entries()[0].bytes, b"first");
        }
    }

    #[test]
    fn hard_tool_call_limit_precedes_mutation() {
        let mut broker = broker();
        let mut limits = limits();
        limits.max_tool_calls = 1;
        let failure = drive(&mut broker, limits, |peer| {
            peer.started();
            peer.send(write_call(10, "call-1", b"first"));
            peer.read();
            peer.send(write_call(11, "call-2", b"second"));
        })
        .unwrap_err();
        assert!(failure.to_string().contains("tool-call limit"));
        assert_eq!(broker.usage().tool_calls, 1);
        assert_eq!(broker.finish().unwrap().tree.entries()[0].bytes, b"first");
    }

    #[test]
    fn output_budget_is_reserved_before_file_mutation() {
        let startup = [
            json!({"id":"pillbox-init","method":"initialize","params":protocol::initialize_params()}),
            json!({"method":"initialized","params":{}}),
            json!({"id":"pillbox-thread","method":"thread/start","params":profile().thread_start(OPERATIONS).unwrap()}),
            json!({"id":"pillbox-turn","method":"turn/start","params":profile().turn_start("thread-1",INPUT).unwrap()}),
        ];
        let mut limits = limits();
        limits.max_output_bytes = startup
            .iter()
            .map(|frame| frame.to_string().len() as u64 + 1)
            .sum();
        let mut broker = broker();
        let failure = drive(&mut broker, limits, |peer| {
            peer.started();
            peer.send(write_call(10, "call-1", b"new"));
        })
        .unwrap_err();
        assert!(failure.to_string().contains("output byte limit"));
        assert_eq!(broker.usage().tool_calls, 0);
        assert!(broker.finish().unwrap().changed_paths.is_empty());
    }

    #[test]
    fn evidence_budget_is_reserved_before_file_mutation() {
        let baseline = drive(&mut broker(), limits(), |peer| {
            peer.started();
            peer.send(write_call(10, "call-1", b"new"));
            peer.read();
            peer.send(terminal());
        })
        .unwrap();
        let prefix_bytes = baseline
            .evidence
            .iter()
            .take_while(|frame| !(frame["direction"] == "outbound" && frame["message"]["id"] == 10))
            .map(|frame| frame.to_string().len() as u64 + 1)
            .sum();
        let mut limits = limits();
        limits.max_evidence_bytes = prefix_bytes;
        let mut broker = broker();
        let failure = drive(&mut broker, limits, |peer| {
            peer.started();
            peer.send(write_call(10, "call-1", b"new"));
        })
        .unwrap_err();
        assert!(failure.to_string().contains("evidence byte limit"));
        assert_eq!(broker.usage().tool_calls, 0);
        assert!(broker.finish().unwrap().changed_paths.is_empty());
    }

    #[test]
    fn malformed_and_ungranted_dynamic_calls_fail_without_leaking_arguments() {
        for case in ["unknown", "arguments", "path", "bytes"] {
            let mut broker = broker();
            let failure = drive(&mut broker, limits(), move |peer| {
                peer.started();
                let mut call = write_call(10, "call-1", b"new");
                match case {
                    "unknown" => call["params"]["tool"] = json!("exec_command"),
                    "arguments" => {
                        call["params"]["arguments"]["executable"] = json!("private file contents")
                    }
                    "path" => call["params"]["arguments"]["path"] = json!("ungranted"),
                    _ => {
                        call["params"]["arguments"]["content"] = json!(BASE64.encode(vec![0; 1025]))
                    }
                }
                peer.send(call);
                let response = peer.read();
                if case == "path" {
                    assert_eq!(response["result"]["success"], false);
                } else {
                    assert_eq!(response["error"]["code"], -32601);
                }
            })
            .unwrap_err();
            assert!(!format!("{failure:?}").contains("private file contents"));
            assert!(!format!("{:?}", failure.error).contains("private file contents"));
            assert!(broker.finish().unwrap().changed_paths.is_empty());
        }
    }

    #[test]
    fn unknown_approval_and_input_requests_receive_typed_denial() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/tool/requestUserInput",
            "unknown/request",
        ] {
            let mut broker = broker();
            let failure = drive(&mut broker, limits(), move |peer| {
                peer.started();
                peer.send(
                    json!({"id":"request-1","method":method,"params":{"private":"do not reflect"}}),
                );
                assert_eq!(
                    peer.read(),
                    protocol::unsupported_server_request(&json!("request-1")).unwrap()
                );
            })
            .unwrap_err();
            assert_eq!(broker.usage().tool_calls, 0);
            assert!(!format!("{failure:?}").contains("do not reflect"));
            assert!(failure
                .evidence
                .iter()
                .any(|entry| entry["message"]["method"] == method));
        }
    }

    #[test]
    fn unknown_notifications_cannot_claim_completion() {
        let mut broker = broker();
        let result = drive(&mut broker,limits(),|peer| {
            peer.started();
            peer.send(json!({"method":"unknown/observation","params":{"status":"completed","text":"fabricated final"}}));
            peer.send(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-1","turnId":"turn-1","delta":"{\"status\":\"completed\"}"}}));
            peer.send(terminal());
        }).unwrap();
        assert_eq!(result.text, "");
        assert!(result
            .evidence
            .iter()
            .any(|entry| entry["message"]["method"] == "unknown/observation"));
    }

    #[test]
    fn mismatched_terminal_and_model_reroute_fail_closed() {
        for case in ["thread", "turn", "failed", "reroute"] {
            let mut broker = broker();
            let failure = drive(&mut broker,limits(),move |peer| {
                peer.started();
                let mut done = terminal();
                match case {
                    "thread" => done["params"]["threadId"] = json!("other"),
                    "turn" => done["params"]["turn"]["id"] = json!("other"),
                    "failed" => {
                        done["params"]["turn"]["status"] = json!("failed");
                        done["params"]["turn"]["error"] = json!({"message":"secret diagnostic"});
                    }
                    _ => done = json!({"method":"model/rerouted","params":{"threadId":"thread-1","turnId":"turn-1"}}),
                }
                peer.send(done);
            }).unwrap_err();
            assert!(!failure.to_string().contains("secret diagnostic"));
            assert!(!failure.evidence.is_empty());
        }
    }

    #[test]
    fn duplicate_rpc_response_is_not_accepted_as_a_new_phase() {
        let mut broker = broker();
        let failure = drive(&mut broker, limits(), |peer| {
            peer.started();
            peer.send(json!({"id":"pillbox-turn","result":turn_result()}));
        })
        .unwrap_err();
        assert!(failure
            .to_string()
            .contains("unexpected native RPC response"));
    }

    #[test]
    fn fragmented_frames_and_exact_frame_limit_work() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let frame = json!({"method":"observation","params":{"text":"Unicode →\nquoted \""}});
        let bytes = frame.to_string().into_bytes();
        let mut limits = limits();
        limits.max_frame_bytes = bytes.len();
        let peer = thread::spawn(move || {
            for byte in bytes {
                server.write_all(&[byte]).unwrap();
            }
            server.write_all(b"\n").unwrap();
        });
        let mut wire = Wire::new(client, limits, || Ok(())).unwrap();
        assert_eq!(wire.receive().unwrap().0, frame);
        peer.join().unwrap();
    }

    #[test]
    fn queued_frames_are_drained_after_peer_disconnect() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let frame = terminal();
        server.write_all(format!("{frame}\n").as_bytes()).unwrap();
        drop(server);
        let mut wire = Wire::new(client, limits(), || Ok(())).unwrap();
        assert_eq!(wire.receive().unwrap().0, frame);
        assert!(wire
            .receive()
            .unwrap_err()
            .to_string()
            .contains("stream closed"));
    }

    #[test]
    fn oversized_unterminated_frame_input_budget_and_evidence_budget_fail() {
        for kind in ["frame", "input", "evidence"] {
            let (client, mut server) = UnixStream::pair().unwrap();
            let mut limits = limits();
            let bytes = match kind {
                "frame" => {
                    limits.max_frame_bytes = 8;
                    b"123456789".to_vec()
                }
                "input" => {
                    limits.max_input_bytes = 8;
                    b"123456789".to_vec()
                }
                _ => {
                    limits.max_evidence_bytes = 1;
                    b"{\"method\":\"notice\"}\n".to_vec()
                }
            };
            server.write_all(&bytes).unwrap();
            let mut wire = Wire::new(client, limits, || Ok(())).unwrap();
            assert!(
                wire.receive().unwrap_err().to_string().contains(kind),
                "{kind}"
            );
            assert!(wire.evidence.is_empty());
        }
    }

    #[test]
    fn malformed_envelopes_remain_actual_failure_evidence() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let frame = json!({"id":1,"result":{},"error":{}});
        server.write_all(format!("{frame}\n").as_bytes()).unwrap();
        let mut wire = Wire::new(client, limits(), || Ok(())).unwrap();
        assert!(wire.receive().is_err());
        assert_eq!(
            wire.evidence,
            vec![json!({"direction":"inbound","message":frame})]
        );
    }

    #[test]
    fn early_notification_queue_is_bounded() {
        let (client, server) = UnixStream::pair().unwrap();
        let peer = thread::spawn(move || {
            let mut peer = Peer::new(server);
            peer.read();
            let frame = b"{\"method\":\"notice\"}\n";
            let batch = frame.repeat(MAX_PENDING_FRAMES + 1);
            peer.writer.write_all(&batch).unwrap();
        });
        let failure = run(
            client,
            &profile(),
            INPUT,
            OPERATIONS,
            &mut broker(),
            limits(),
            || Ok(()),
        )
        .unwrap_err();
        assert!(
            failure.to_string().contains("pending-frame limit"),
            "unexpected failure: {failure}"
        );
        peer.join().unwrap();
    }

    #[test]
    fn cancellation_is_polled_while_waiting_for_a_native_response() {
        let (client, server) = UnixStream::pair().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let peer_cancelled = Arc::clone(&cancelled);
        let peer = thread::spawn(move || {
            let mut peer = Peer::new(server);
            peer.read();
            peer_cancelled.store(true, Ordering::SeqCst);
            let mut byte = [0];
            assert_eq!(peer.reader.get_mut().read(&mut byte).unwrap(), 0);
        });
        let result = run(
            client,
            &profile(),
            INPUT,
            OPERATIONS,
            &mut broker(),
            limits(),
            move || {
                ensure!(!cancelled.load(Ordering::SeqCst), "cancelled");
                Ok(())
            },
        )
        .unwrap_err();
        assert!(result.to_string().contains("cancelled"));
        assert_eq!(result.evidence.len(), 1);
        peer.join().unwrap();
    }

    #[test]
    fn deadlines_bound_silent_reads_and_blocked_writes() {
        for write in [false, true] {
            let (client, _server) = UnixStream::pair().unwrap();
            let mut limits = limits();
            limits.deadline = Instant::now() + Duration::from_millis(40);
            let mut wire = Wire::new(client, limits, || Ok(())).unwrap();
            let start = Instant::now();
            let error = if write {
                wire.send(json!({"method":"large","params":{"text":"x".repeat(900_000)}}))
                    .unwrap_err()
            } else {
                wire.receive().unwrap_err()
            };
            assert!(error.to_string().contains("deadline"));
            assert!(start.elapsed() < Duration::from_secs(1));
            assert!(wire.evidence.is_empty());
        }
    }

    #[test]
    fn pinned_server_notifications_preserve_emission_timestamps_without_authority() {
        let result = drive(&mut broker(), limits(), |peer| {
            peer.started();
            peer.send(json!({"emittedAtMs":1790156306027_i64,
                "method":"remoteControl/status/changed", "params":{"status":"disabled"}}));
            let mut done = terminal();
            done["emittedAtMs"] = json!(1790156306028_i64);
            peer.send(done);
        })
        .unwrap();
        assert!(result
            .evidence
            .iter()
            .any(|frame| frame["message"]["emittedAtMs"] == 1790156306027_i64));
        assert_eq!(result.turn_id, "turn-1");
        for frame in [
            json!({"id":1,"result":{},"emittedAtMs":1}),
            json!({"id":1,"method":"request","params":{},"emittedAtMs":1}),
            json!({"method":"notice","params":{},"emittedAtMs":"1"}),
            json!({"method":"notice","params":{},"emittedAtMs":null}),
            json!({"method":"notice","params":{},"emittedAtMs":1.5}),
            json!({"method":"notice","params":{},"emittedAtMs":18446744073709551615_u64}),
        ] {
            assert!(validate_envelope(&frame).is_err());
        }
    }

    #[test]
    fn limits_and_ambiguous_envelopes_fail_before_use() {
        let mut invalid = limits();
        invalid.max_tool_calls = MAX_TOOL_CALLS + 1;
        assert!(invalid.validate().is_err());
        for frame in [
            json!([]),
            json!({"id":1,"result":{},"error":{}}),
            json!({"id":1,"method":"request","result":{}}),
            json!({"method":"notice","params":[]}),
            json!({"method":"notice","extra":true}),
        ] {
            assert!(validate_envelope(&frame).is_err());
        }
    }
}
