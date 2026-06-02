//! Map opencode's `/event` SSE envelopes into pillbox §0 `contract::Payload`s.
//!
//! opencode is **structured-API-native**: `opencode serve` exposes a headless
//! HTTP server whose `/event` endpoint streams typed events, so instead of
//! scraping a transcript file (the claude/codex `transcripts` path) we consume
//! its event stream directly — structured, real-time, no file-tailing. This
//! module is the pure mapping core; the SSE transport + the bridge that feeds
//! the durable [`SessionLog`](crate::events::log::SessionLog) live in the
//! sandbox run path.
//!
//! ## Which events carry the turn
//!
//! Verified against a live GLM turn through `opencode serve` (not just the
//! OpenAPI): the assistant turn streams over the **`message.*` family** —
//!
//! - `message.updated` → `info:{id, role, model}` — a message was created /
//!   updated. The first sight of an `assistant` message id opens it.
//! - `message.part.delta` → `{messageID, field, delta}` — incremental content
//!   (`field:"text"` = assistant text; `field:"reasoning"` = thinking).
//! - `message.part.updated` with `part.type == "tool"` → `{tool, callID,
//!   state:{status, input, output}}` — a tool call's evolving state.
//! - `session.idle` → the turn went quiescent (end the open message + the
//!   `NeedsInput` attention signal, matching the claude end_turn producer).
//!
//! The parallel `session.next.*` family exists in the OpenAPI but only emitted
//! lifecycle bits (`agent.switched`, `model.switched`) in practice — it is
//! *not* the content source, so we ignore it (along with the `text`/`reasoning`
//! part *snapshots*, whose content the deltas already carry, and `step-*`,
//! `session.{updated,status,diff}`, `server.*`).
//!
//! Stateful: deltas carry a `messageID` but no role, so we track which message
//! ids we've opened as assistant (emit `MessageStart` once each); a tool's
//! status is emitted only when it *changes* (`pending`→`running`→`completed`)
//! so a chatty input-stream doesn't flood the log with duplicate `ToolCall`s.

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{
    AttentionReason, AttentionRequired, Event, MessageDelta, MessageEnd, MessageStart, Payload,
    Role, Thinking, ToolCall, ToolStatus,
};
use crate::events::log::SessionLog;

/// Stateful opencode-event → §0-payload mapper. One per session stream.
#[derive(Default)]
pub(crate) struct EventMapper {
    /// The currently-open assistant message id (set on the first `message.updated`
    /// for an assistant message, cleared on `session.idle`). `message.updated`
    /// fires repeatedly for the same message; comparing against this suppresses
    /// duplicate `MessageStart`s without an ever-growing seen-set, since opencode
    /// opens exactly one assistant message per turn (a new id only after idle).
    open_msg: Option<String>,
    /// `callID → last emitted tool status`, so we only emit a `ToolCall` when a
    /// tool's status actually changes, not on every input-stream tick. Keyed on
    /// the *mapped* status so opencode's `pending`→`running` (both `Running`)
    /// collapses to one event.
    tool_status: HashMap<String, ToolStatus>,
}

impl EventMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Map one opencode `/event` envelope into zero or more §0 payloads.
    /// Unmapped types return empty (the stream carries far more than the turn —
    /// lifecycle, sync, tui, lsp, step boundaries, …).
    pub(crate) fn on_event(&mut self, ev: &Value) -> Vec<Payload> {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or_default();
        let p = ev.get("properties").unwrap_or(&Value::Null);

        match ty {
            "message.updated" => self.on_message_updated(p),
            "message.part.delta" => self.on_part_delta(p),
            "message.part.updated" => self.on_part_updated(p),
            // Turn went quiescent → close the open assistant message and raise
            // the attention signal the driver waits on.
            "session.idle" => {
                let mut out = Vec::new();
                if let Some(id) = self.open_msg.take() {
                    out.push(Payload::MessageEnd(MessageEnd::new(id)));
                }
                out.push(attention(AttentionReason::NeedsInput));
                out
            }
            "permission.asked" => vec![attention(AttentionReason::Permission)],
            "question.asked" => vec![attention(AttentionReason::NeedsInput)],
            "session.error" => vec![Payload::AttentionRequired(AttentionRequired {
                reason: AttentionReason::ErrorStalled,
                message: error_message(p),
            })],
            _ => vec![],
        }
    }

    /// `message.updated` — open an assistant message on its first sighting.
    /// User messages (the echo of our own prompt) and repeat updates of an
    /// already-open message produce nothing.
    fn on_message_updated(&mut self, p: &Value) -> Vec<Payload> {
        let info = p.get("info").unwrap_or(&Value::Null);
        let role = info.get("role").and_then(Value::as_str).unwrap_or_default();
        let id = info.get("id").and_then(Value::as_str).unwrap_or_default();
        if role != "assistant" || id.is_empty() || self.open_msg.as_deref() == Some(id) {
            return vec![];
        }
        self.open_msg = Some(id.to_string());
        vec![Payload::MessageStart(MessageStart {
            message_id: id.to_string(),
            role: Role::Assistant,
        })]
    }

    /// `message.part.delta` — the streaming content. `field` selects the §0
    /// channel: assistant text vs. reasoning/thinking. Empty deltas drop.
    fn on_part_delta(&mut self, p: &Value) -> Vec<Payload> {
        let delta = p.get("delta").and_then(Value::as_str).unwrap_or_default();
        if delta.is_empty() {
            return vec![];
        }
        match p.get("field").and_then(Value::as_str).unwrap_or("text") {
            "reasoning" => vec![Payload::Thinking(Thinking {
                text: delta.to_string(),
            })],
            // Default to text (the common case; opencode's deltas are `text`).
            _ => {
                // Attach to the delta's own messageID, falling back to the open
                // assistant message. If neither exists there's nothing to attach
                // to — drop it rather than emit a delta with an empty id.
                let Some(message_id) = p
                    .get("messageID")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.open_msg.clone())
                else {
                    return vec![];
                };
                vec![Payload::MessageDelta(MessageDelta {
                    message_id,
                    text: delta.to_string(),
                })]
            }
        }
    }

    /// `message.part.updated` — only the `tool` parts are mapped (text/reasoning
    /// part snapshots duplicate the deltas; step-start/finish are boundaries).
    /// Emits a `ToolCall` only when the tool's status changes.
    fn on_part_updated(&mut self, p: &Value) -> Vec<Payload> {
        let part = p.get("part").unwrap_or(&Value::Null);
        if part.get("type").and_then(Value::as_str) != Some("tool") {
            return vec![];
        }
        let call_id = part
            .get("callID")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let state = part.get("state").unwrap_or(&Value::Null);
        let status = map_tool_status(
            state
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("running"),
        );
        // De-dupe: a tool part updates repeatedly as its input streams; only the
        // mapped-status transitions (Running → Completed/Error) are interesting.
        if self.tool_status.get(&call_id) == Some(&status) {
            return vec![];
        }
        self.tool_status.insert(call_id.clone(), status);
        vec![Payload::ToolCall(ToolCall {
            tool_call_id: call_id,
            name: part
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            status,
            input: state.get("input").filter(|v| !v.is_null()).cloned(),
            output: state
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            title: String::new(),
        })]
    }
}

fn map_tool_status(s: &str) -> ToolStatus {
    match s {
        "completed" => ToolStatus::Completed,
        "error" => ToolStatus::Error,
        // pending / running / anything mid-flight
        _ => ToolStatus::Running,
    }
}

fn attention(reason: AttentionReason) -> Payload {
    Payload::AttentionRequired(AttentionRequired {
        reason,
        message: String::new(),
    })
}

/// Pull a message out of a `session.error` event's `error` (string, or an object
/// with `message` / `data.message`).
fn error_message(props: &Value) -> String {
    match props.get("error") {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => obj
            .get("message")
            .or_else(|| obj.get("data").and_then(|d| d.get("message")))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Drain an opencode `/event` SSE stream into the durable [`SessionLog`],
/// mapping each event through [`EventMapper`]. The transport-agnostic core: the
/// caller hands a reader (a live HTTP body, or a `Cursor` in tests) and a stop
/// flag; we parse SSE frames (`data:` lines terminated by a blank line), map
/// each JSON envelope, and append the resulting §0 events.
///
/// Blocks reading the stream until it closes or `stop` is set (observed between
/// frames — a live caller closes the connection to unblock). Non-JSON `data:`
/// payloads and unmapped event types are skipped, not errored, so a stray frame
/// can't wedge the stream. Returns the number of §0 events appended.
pub(crate) fn drain_sse<R: std::io::Read>(
    reader: R,
    session_id: &str,
    log: &mut SessionLog,
    stop: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<usize> {
    use std::io::BufRead as _;
    use std::sync::atomic::Ordering;

    let mut mapper = EventMapper::new();
    let mut data = String::new();
    let mut total = 0;
    let mut lines = std::io::BufReader::new(reader).lines();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(line) = lines.next() else { break };
        let line = line?;
        if let Some(rest) = line.strip_prefix("data:") {
            // SSE allows one optional space after the colon; multiple `data:`
            // lines in a frame join with newlines.
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() {
            // Blank line ends the frame.
            total += flush_frame(&mut mapper, &mut data, session_id, log)?;
        }
        // `event:` / `id:` / `retry:` / `:comment` lines carry no payload here.
    }
    // A stream that closed mid-frame (no trailing blank line) still flushes.
    total += flush_frame(&mut mapper, &mut data, session_id, log)?;
    Ok(total)
}

/// Map one accumulated SSE `data` payload (cleared afterward) into §0 events and
/// append them. A non-JSON or unmapped frame appends nothing.
fn flush_frame(
    mapper: &mut EventMapper,
    data: &mut String,
    session_id: &str,
    log: &mut SessionLog,
) -> anyhow::Result<usize> {
    if data.is_empty() {
        return Ok(0);
    }
    let parsed: Result<Value, _> = serde_json::from_str(data);
    data.clear();
    let Ok(value) = parsed else { return Ok(0) };
    let events: Vec<Event> = mapper
        .on_event(&value)
        .into_iter()
        .map(|p| Event::session(session_id, p))
        .collect();
    if events.is_empty() {
        return Ok(0);
    }
    let n = events.len();
    log.append(&events)?;
    Ok(n)
}

/// A [`Read`](std::io::Read) over a growing file that **blocks at EOF** (polling)
/// instead of ending — so `drain_sse` follows the in-sandbox `/event` capture
/// file like `tail -F` (replay everything already there, then stream appends).
/// Returns `Ok(0)` (real EOF → ends the drain) only once `stop` is set, so the
/// owning [`TailerHandle`](crate::events::transcripts::TailerHandle) shuts it
/// down within one poll interval. Reading a file being appended is safe: at EOF
/// the offset holds, and a later read returns bytes written past it. (Consumed
/// by the libkrun file path; docker §0 still uses the live bridge.)
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) struct FollowReader {
    file: std::fs::File,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FollowReader {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn new(
        file: std::fs::File,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self { file, stop }
    }
}

impl std::io::Read for FollowReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::atomic::Ordering;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let n = self.file.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(ty: &str, props: Value) -> Value {
        json!({ "id": "evt_x", "type": ty, "properties": props })
    }

    // Envelope shapes below are trimmed from a real GLM turn captured through
    // `opencode serve` (opencode 1.15.10), so the mapper is tested against the
    // wire format, not a guess.

    #[test]
    fn assistant_message_then_text_deltas_then_idle() {
        let mut m = EventMapper::new();

        // The user-message echo of our own prompt maps to nothing.
        assert!(m
            .on_event(&ev(
                "message.updated",
                json!({ "sessionID": "ses_a", "info": { "id": "msg_u", "role": "user" } }),
            ))
            .is_empty());

        // First sight of the assistant message → MessageStart.
        let start = m.on_event(&ev(
            "message.updated",
            json!({ "sessionID": "ses_a", "info": {
                "id": "msg_a", "role": "assistant",
                "model": { "providerID": "zai-coding-plan", "modelID": "glm-4.5-air" } } }),
        ));
        assert!(matches!(&start[..],
            [Payload::MessageStart(s)] if s.message_id == "msg_a" && s.role == Role::Assistant));
        // A repeat update of the same message does NOT re-open it.
        assert!(m
            .on_event(&ev(
                "message.updated",
                json!({ "sessionID": "ses_a", "info": { "id": "msg_a", "role": "assistant" } }),
            ))
            .is_empty());

        // Streaming text deltas carry the messageID.
        let d = m.on_event(&ev(
            "message.part.delta",
            json!({ "sessionID": "ses_a", "messageID": "msg_a", "partID": "prt_1",
                    "field": "text", "delta": "hi" }),
        ));
        assert!(matches!(&d[..],
            [Payload::MessageDelta(x)] if x.text == "hi" && x.message_id == "msg_a"));
        // Empty deltas drop.
        assert!(m
            .on_event(&ev(
                "message.part.delta",
                json!({ "sessionID": "ses_a", "messageID": "msg_a", "field": "text", "delta": "" }),
            ))
            .is_empty());

        // Idle ends the open message and raises NeedsInput.
        let idle = m.on_event(&ev("session.idle", json!({ "sessionID": "ses_a" })));
        assert!(matches!(&idle[..],
            [Payload::MessageEnd(e), Payload::AttentionRequired(a)]
            if e.message_id == "msg_a" && a.reason == AttentionReason::NeedsInput));
    }

    #[test]
    fn reasoning_delta_maps_to_thinking() {
        let mut m = EventMapper::new();
        let t = m.on_event(&ev(
            "message.part.delta",
            json!({ "sessionID": "s", "messageID": "m", "field": "reasoning", "delta": "hmm" }),
        ));
        assert!(matches!(&t[..], [Payload::Thinking(x)] if x.text == "hmm"));
    }

    #[test]
    fn tool_part_emits_on_status_change_only() {
        let mut m = EventMapper::new();
        let tool = |status: &str, input: Value| {
            ev(
                "message.part.updated",
                json!({ "sessionID": "s", "part": {
                    "id": "prt_t", "messageID": "m", "type": "tool",
                    "tool": "ls", "callID": "call_1",
                    "state": { "status": status, "input": input } } }),
            )
        };
        // pending → Running (with name + input).
        let a = m.on_event(&tool("pending", json!({ "path": "." })));
        assert!(matches!(&a[..], [Payload::ToolCall(t)]
            if t.name == "ls" && t.tool_call_id == "call_1" && t.status == ToolStatus::Running
               && t.input.as_ref().and_then(|i| i.get("path")).and_then(|x| x.as_str()) == Some(".")));
        // running → still Running, but status unchanged from our mapping → no dup.
        assert!(m
            .on_event(&tool("running", json!({ "path": "." })))
            .is_empty());
        // completed → Completed with output.
        let done = ev(
            "message.part.updated",
            json!({ "sessionID": "s", "part": {
                "type": "tool", "tool": "ls", "callID": "call_1",
                "state": { "status": "completed", "output": "a\nb" } } }),
        );
        assert!(matches!(&m.on_event(&done)[..],
            [Payload::ToolCall(t)] if t.status == ToolStatus::Completed && t.output == "a\nb"));
    }

    #[test]
    fn snapshots_and_lifecycle_and_session_next_are_ignored() {
        let mut m = EventMapper::new();
        // text/reasoning part *snapshots* (deltas already carry their content),
        // step boundaries, session lifecycle, the session.next.* family, server.*
        for e in [
            ev(
                "message.part.updated",
                json!({ "part": { "type": "text", "text": "full text so far" } }),
            ),
            ev(
                "message.part.updated",
                json!({ "part": { "type": "step-finish", "tokens": { "total": 10 } } }),
            ),
            ev(
                "session.next.text.delta",
                json!({ "sessionID": "s", "delta": "x" }),
            ),
            ev("session.next.model.switched", json!({ "sessionID": "s" })),
            ev("session.updated", json!({ "sessionID": "s" })),
            ev("server.heartbeat", json!({})),
        ] {
            assert!(m.on_event(&e).is_empty(), "should ignore: {}", e["type"]);
        }
    }

    /// End-to-end transport: a raw `/event` SSE byte stream (a text turn that
    /// goes idle) drains into the durable log as the mapped §0 events — the same
    /// sink `session watch`/`subscribe` read, so opencode lights up there with
    /// no transcript file.
    #[test]
    fn drain_sse_feeds_the_durable_log() {
        use std::io::Cursor;
        use std::sync::atomic::AtomicBool;

        crate::test_util::with_isolated_home("opencode-drain-sse", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "ses-oc").expect("open log");

            let stream = "\
data: {\"type\":\"server.connected\",\"properties\":{}}\n\
\n\
data: {\"type\":\"message.updated\",\"properties\":{\"info\":{\"id\":\"msg_a\",\"role\":\"assistant\"}}}\n\
\n\
data: {\"type\":\"message.part.delta\",\"properties\":{\"messageID\":\"msg_a\",\"field\":\"text\",\"delta\":\"hi \"}}\n\
\n\
data: {\"type\":\"message.part.delta\",\"properties\":{\"messageID\":\"msg_a\",\"field\":\"text\",\"delta\":\"there\"}}\n\
\n\
data: {\"type\":\"session.idle\",\"properties\":{\"sessionID\":\"ses_oc\"}}\n\
\n";

            let stop = AtomicBool::new(false);
            let n = drain_sse(Cursor::new(stream), "ses-oc", &mut log, &stop).expect("drain");
            assert_eq!(n, 5, "start + 2 deltas + end + attention");

            let events = SessionLog::open(&pb, "ses-oc")
                .unwrap()
                .read_from(0)
                .unwrap();
            use crate::contract::Payload as P;
            assert!(matches!(events[0].payload, P::MessageStart(_)));
            assert!(matches!(&events[1].payload, P::MessageDelta(d) if d.text == "hi "));
            assert!(matches!(&events[2].payload, P::MessageDelta(d) if d.text == "there"));
            assert!(matches!(events[3].payload, P::MessageEnd(_)));
            assert!(matches!(&events[4].payload,
                P::AttentionRequired(a) if a.reason == AttentionReason::NeedsInput));
            assert_eq!(
                events.iter().map(|e| e.seq).collect::<Vec<_>>(),
                vec![1, 2, 3, 4, 5]
            );
        });
    }
}
