//! Map OpenCode's `/api/event` SSE envelopes into pillbox §0 `contract::Payload`s.
//!
//! opencode is **structured-API-native**: `opencode serve` exposes a headless
//! HTTP server whose `/api/event` endpoint streams typed events, so instead of
//! scraping a transcript file (the claude/codex `transcripts` path) we consume
//! its event stream directly. This module is the pure mapping core; the SSE
//! transport + the bridge that feeds the durable
//! [`SessionLog`](crate::events::log::SessionLog) live in the sandbox run path.
//!
//! ## Which events carry the turn (OpenCode 2)
//!
//! Verified against captured turns from `opencode serve` 2.0.24 (a write + shell
//! tool round-trip, and a provider failure), not only the published types. The
//! envelope is `{id, created, type, location?, durable?, data:{sessionID, …}}`;
//! content streams per model step, and each step is its own assistant message
//! (`assistantMessageID`):
//!
//! - `session.text.started` / `.delta` / `.ended` → `MessageStart` /
//!   `MessageDelta` (the deltas are canonical; `.ended` repeats the whole text
//!   and is only used when no delta arrived).
//! - `session.reasoning.delta` (or `.ended` without deltas) → `Thinking`.
//! - `session.tool.input.started {id, name}` names the call;
//!   `session.tool.called {id, input}` → `ToolCall{Running}`;
//!   `session.tool.success {id, content}` → `ToolCall{Completed}`;
//!   `session.tool.failed {id, error}` → `ToolCall{Error}`.
//! - `session.step.ended` / `session.step.failed` → `Usage` (`source: native`)
//!   from `tokens`/`cost`, and closes that step's message.
//! - `session.execution.succeeded` → the turn ended: `AttentionRequired{NeedsInput}`.
//!   `session.execution.failed` / `.interrupted` → `AttentionRequired{ErrorStalled}`
//!   (the codex-serve convention). OpenCode 2 declares `session.idle` but did
//!   not emit it in the captured turns, so it is not the boundary.
//! - `permission.asked` → `Permission`; `form.created` (a question for the
//!   user) → `NeedsInput`.
//!
//! Everything else (`session.inbox.*`, `session.instructions.updated`,
//! `session.step.started`/`.streamed`, `session.tool.input.delta`/`.progress`,
//! `session.usage.updated` running totals, `shell.*`, `server.*`) is ignored.
//! The OpenCode 1 `message.*` family no longer exists.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::contract::{
    Actor, AttentionReason, AttentionRequired, Event, MessageDelta, MessageEnd, MessageStart,
    Payload, Role, Thinking, ToolCall, ToolStatus, Usage, UsageSource,
};
use crate::events::log::SessionLog;

/// Stateful opencode-event → §0-payload mapper. One per session stream.
#[derive(Default)]
pub(crate) struct EventMapper {
    /// Assistant message ids opened with a `MessageStart` and not yet ended.
    open_msgs: Vec<String>,
    /// `(assistantMessageID, ordinal)` text blocks that streamed at least one
    /// delta, so a block's `.ended` (which repeats the whole text) is skipped.
    text_streamed: HashSet<(String, u64)>,
    /// Same, for reasoning blocks.
    reasoning_streamed: HashSet<(String, u64)>,
    /// Tool call id → tool name (`session.tool.input.started` is the only event
    /// that names the tool).
    tool_names: HashMap<String, String>,
    /// Tool call id → input (from `session.tool.called`), echoed on the result.
    tool_inputs: HashMap<String, Value>,
}

impl EventMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Map one opencode `/api/event` envelope into zero or more §0 payloads.
    /// Unmapped types return empty (the stream carries far more than the turn).
    pub(crate) fn on_event(&mut self, ev: &Value) -> Vec<Payload> {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or_default();
        let d = ev.get("data").unwrap_or(&Value::Null);

        match ty {
            "session.text.started" => self.open(msg_id(d)).into_iter().collect(),
            "session.text.delta" => self.on_text_delta(d),
            "session.text.ended" => self.on_text_ended(d),
            "session.reasoning.delta" => {
                self.reasoning_streamed.insert(block_key(d));
                thinking(str_of(d, "delta"))
            }
            "session.reasoning.ended" => {
                if self.reasoning_streamed.remove(&block_key(d)) {
                    vec![]
                } else {
                    thinking(str_of(d, "text"))
                }
            }
            "session.tool.input.started" => {
                self.tool_names
                    .insert(str_of(d, "id").to_string(), str_of(d, "name").to_string());
                vec![]
            }
            "session.tool.called" => self.on_tool_called(d),
            "session.tool.success" => {
                let output = tool_content_text(d.get("content"));
                self.on_tool_finished(d, ToolStatus::Completed, output)
            }
            "session.tool.failed" => {
                let output = structured_error_message(d.get("error"));
                self.on_tool_finished(d, ToolStatus::Error, output)
            }
            "session.step.ended" | "session.step.failed" => {
                let mut out: Vec<Payload> =
                    usage_from_step(d).map(Payload::Usage).into_iter().collect();
                out.extend(self.close(msg_id(d)));
                out
            }
            "session.execution.succeeded" => {
                self.end_turn(AttentionReason::NeedsInput, String::new())
            }
            "session.execution.failed" => {
                let message = structured_error_message(d.get("error"));
                self.end_turn(AttentionReason::ErrorStalled, message)
            }
            "session.execution.interrupted" => {
                let message = format!("interrupted ({})", str_of(d, "reason"));
                self.end_turn(AttentionReason::ErrorStalled, message)
            }
            "permission.asked" => vec![attention(AttentionReason::Permission, String::new())],
            "form.created" => vec![attention(AttentionReason::NeedsInput, String::new())],
            _ => vec![],
        }
    }

    /// Open `id` as an assistant message (once). An empty id opens nothing.
    fn open(&mut self, id: &str) -> Option<Payload> {
        if id.is_empty() || self.open_msgs.iter().any(|m| m == id) {
            return None;
        }
        self.open_msgs.push(id.to_string());
        Some(Payload::MessageStart(MessageStart {
            message_id: id.to_string(),
            role: Role::Assistant,
        }))
    }

    /// Close `id` if it is open.
    fn close(&mut self, id: &str) -> Option<Payload> {
        let pos = self.open_msgs.iter().position(|m| m == id)?;
        let id = self.open_msgs.remove(pos);
        Some(Payload::MessageEnd(MessageEnd::new(id)))
    }

    fn on_text_delta(&mut self, d: &Value) -> Vec<Payload> {
        let delta = str_of(d, "delta");
        let id = msg_id(d);
        if delta.is_empty() || id.is_empty() {
            return vec![];
        }
        self.text_streamed.insert(block_key(d));
        let mut out: Vec<Payload> = self.open(id).into_iter().collect();
        out.push(Payload::MessageDelta(MessageDelta {
            message_id: id.to_string(),
            text: delta.to_string(),
        }));
        out
    }

    /// `.ended` repeats the block's whole text: only a block that streamed no
    /// delta contributes it.
    fn on_text_ended(&mut self, d: &Value) -> Vec<Payload> {
        if self.text_streamed.remove(&block_key(d)) {
            return vec![];
        }
        let text = str_of(d, "text");
        let id = msg_id(d);
        if text.is_empty() || id.is_empty() {
            return vec![];
        }
        let mut out: Vec<Payload> = self.open(id).into_iter().collect();
        out.push(Payload::MessageDelta(MessageDelta {
            message_id: id.to_string(),
            text: text.to_string(),
        }));
        out
    }

    fn on_tool_called(&mut self, d: &Value) -> Vec<Payload> {
        let call_id = str_of(d, "id").to_string();
        let input = d.get("input").filter(|v| !v.is_null()).cloned();
        if let Some(input) = &input {
            self.tool_inputs.insert(call_id.clone(), input.clone());
        }
        vec![Payload::ToolCall(ToolCall {
            name: self.tool_names.get(&call_id).cloned().unwrap_or_default(),
            tool_call_id: call_id,
            status: ToolStatus::Running,
            input,
            output: String::new(),
            title: String::new(),
        })]
    }

    fn on_tool_finished(&mut self, d: &Value, status: ToolStatus, output: String) -> Vec<Payload> {
        let call_id = str_of(d, "id").to_string();
        vec![Payload::ToolCall(ToolCall {
            name: self.tool_names.remove(&call_id).unwrap_or_default(),
            input: self.tool_inputs.remove(&call_id),
            tool_call_id: call_id,
            status,
            output,
            title: String::new(),
        })]
    }

    /// The turn ended: close every open message, then raise the attention
    /// signal drivers wait on.
    fn end_turn(&mut self, reason: AttentionReason, message: String) -> Vec<Payload> {
        let mut out: Vec<Payload> = self
            .open_msgs
            .drain(..)
            .map(|id| Payload::MessageEnd(MessageEnd::new(id)))
            .collect();
        self.text_streamed.clear();
        self.reasoning_streamed.clear();
        out.push(attention(reason, message));
        out
    }
}

fn str_of<'a>(d: &'a Value, key: &str) -> &'a str {
    d.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn msg_id(d: &Value) -> &str {
    str_of(d, "assistantMessageID")
}

fn block_key(d: &Value) -> (String, u64) {
    (
        msg_id(d).to_string(),
        d.get("ordinal").and_then(Value::as_u64).unwrap_or(0),
    )
}

fn thinking(text: &str) -> Vec<Payload> {
    if text.is_empty() {
        return vec![];
    }
    vec![Payload::Thinking(Thinking {
        text: text.to_string(),
    })]
}

/// Join a tool result's text content parts (`[{type:"text", text}]`).
fn tool_content_text(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Map a finished step's `tokens`/`cost` into a §0 [`Usage`] (`source:
/// native`). `None` when the step carries no modelled token field.
fn usage_from_step(d: &Value) -> Option<Usage> {
    let tokens = d.get("tokens")?;
    let count = |obj: &Value, k: &str| obj.get(k).and_then(Value::as_u64);
    let cache = tokens.get("cache").unwrap_or(&Value::Null);
    let input = count(tokens, "input");
    let output = count(tokens, "output");
    let cache_read = count(cache, "read");
    let cache_creation = count(cache, "write");
    input.or(output).or(cache_read).or(cache_creation)?;
    Some(Usage {
        message_id: msg_id(d).to_string(),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
        cost_usd: d.get("cost").and_then(Value::as_f64),
        source: UsageSource::Native,
    })
}

fn attention(reason: AttentionReason, message: String) -> Payload {
    Payload::AttentionRequired(AttentionRequired { reason, message })
}

/// OpenCode 2's structured error is `{type, message}`; fall back to the type
/// when there is no message.
fn structured_error_message(error: Option<&Value>) -> String {
    match error {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => {
            let message = str_of(obj, "message");
            if message.is_empty() {
                str_of(obj, "type").to_string()
            } else {
                message.to_string()
            }
        }
        _ => String::new(),
    }
}

/// Drain an opencode `/api/event` SSE stream into the durable [`SessionLog`],
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
        // `lines()` strips `\n` but keeps a `\r` — tolerate CRLF SSE so a blank
        // `\r` line still terminates a frame and a `data:…\r` doesn't carry the
        // `\r` into the JSON. (opencode emits bare `\n` today; this is a guard.)
        let line = line.trim_end_matches('\r');
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
    // opencode's `/api/event` stream is the agent's own output — stamp it `agent`
    // (the host knows it launched opencode; the guest can't claim a different actor).
    let events: Vec<Event> = mapper
        .on_event(&value)
        .into_iter()
        .map(|p| Event::session(session_id, p).with_actor(Actor::agent("opencode")))
        .collect();
    if events.is_empty() {
        return Ok(0);
    }
    let n = events.len();
    log.append(&events)?;
    Ok(n)
}

/// A [`Read`](std::io::Read) over a growing file that **blocks at EOF** (polling)
/// instead of ending — so `drain_sse` follows the in-sandbox `/api/event` capture
/// file like `tail -F` (replay everything already there, then stream appends).
/// Reading a file being appended is safe: at EOF the offset holds, and a later
/// read returns bytes written past it. (Consumed by the libkrun file path;
/// docker §0 still uses the live bridge.)
///
/// Two subtleties the obvious version gets wrong:
/// - **Opens lazily by path.** The guest creates the file only when opencode
///   emits its first SSE line, so a `watch` right after `run` can beat it; we
///   poll for the file to appear rather than giving up (which would silently
///   capture nothing for the run-then-watch ordering).
/// - **Reads before checking `stop`.** On stop we do a final read first, so any
///   frames the guest flushed during the last poll sleep are still drained
///   (mirrors the file tailer's final-pump); only a genuine EOF *and* `stop`
///   ends the drain.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) struct FollowReader {
    path: std::path::PathBuf,
    file: Option<std::fs::File>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FollowReader {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn new(
        path: std::path::PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            path,
            file: None,
            stop,
        }
    }
}

impl std::io::Read for FollowReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::atomic::Ordering;
        let nap = std::time::Duration::from_millis(200);
        loop {
            // Lazy open: wait for the guest to create the file (first SSE line).
            if self.file.is_none() {
                if self.stop.load(Ordering::Relaxed) {
                    return Ok(0);
                }
                match std::fs::File::open(&self.path) {
                    Ok(f) => self.file = Some(f),
                    Err(_) => {
                        std::thread::sleep(nap);
                        continue;
                    }
                }
            }
            // Read FIRST, then decide on `stop` — so a final read after stop is
            // observed still drains frames flushed during the previous nap.
            let n = self.file.as_mut().expect("opened above").read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            if self.stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            std::thread::sleep(nap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::atomic::AtomicBool;

    /// A real OpenCode 2.0.24 turn (free Zen model): write `hello.txt`, run
    /// `cat hello.txt` through the shell tool, answer. Paths sanitized.
    const TOOL_TURN: &str = include_str!("fixtures/opencode-2.0.24-tool-turn.sse");
    /// A real OpenCode 2.0.24 turn that failed before sampling (no route to the
    /// requested model).
    const FAILED_TURN: &str = include_str!("fixtures/opencode-2.0.24-failed-turn.sse");

    fn ev(ty: &str, data: Value) -> Value {
        json!({ "id": "evt_x", "created": 1, "type": ty, "data": data })
    }

    /// Map every frame of a captured SSE stream (no log), in order.
    fn map_capture(sse: &str) -> Vec<Payload> {
        let mut m = EventMapper::new();
        sse.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .flat_map(|v| m.on_event(&v))
            .collect()
    }

    #[test]
    fn captured_tool_turn_maps_tools_text_usage_and_the_boundary() {
        let out = map_capture(TOOL_TURN);

        let tools: Vec<&ToolCall> = out
            .iter()
            .filter_map(|p| match p {
                Payload::ToolCall(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(
            tools.len(),
            4,
            "two calls, each Running then Completed: {tools:?}"
        );
        assert_eq!(
            (tools[0].name.as_str(), tools[0].status),
            ("write", ToolStatus::Running)
        );
        assert_eq!(
            tools[0].input,
            Some(json!({"path": "hello.txt", "content": "hi"}))
        );
        assert_eq!(tools[1].status, ToolStatus::Completed);
        assert_eq!(tools[1].tool_call_id, tools[0].tool_call_id);
        assert_eq!(tools[1].output, "Created file successfully: hello.txt");
        assert_eq!(
            (tools[2].name.as_str(), tools[2].status),
            ("shell", ToolStatus::Running)
        );
        assert_eq!(
            (
                tools[3].name.as_str(),
                tools[3].status,
                tools[3].output.as_str()
            ),
            ("shell", ToolStatus::Completed, "hi")
        );

        let text: String = out
            .iter()
            .filter_map(|p| match p {
                Payload::MessageDelta(d) => Some(d.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "`cat hello.txt` printed:\n\n```\nhi\n```",
            "deltas only, no .ended repeat"
        );
        assert!(out.iter().any(|p| matches!(p, Payload::Thinking(_))));

        let usage: Vec<&Usage> = out
            .iter()
            .filter_map(|p| match p {
                Payload::Usage(u) => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(usage.len(), 3, "one per model step");
        assert_eq!(usage[0].input_tokens, Some(5526));
        assert_eq!(usage[2].cache_read_input_tokens, Some(4352));
        assert!(usage.iter().all(|u| u.source == UsageSource::Native));

        // Every message that started also ended, and the turn ends on NeedsInput.
        let starts = out
            .iter()
            .filter(|p| matches!(p, Payload::MessageStart(_)))
            .count();
        let ends = out
            .iter()
            .filter(|p| matches!(p, Payload::MessageEnd(_)))
            .count();
        assert_eq!((starts, ends), (1, 1));
        assert!(matches!(out.last(),
            Some(Payload::AttentionRequired(a)) if a.reason == AttentionReason::NeedsInput));
    }

    #[test]
    fn captured_failed_turn_raises_error_stalled_with_the_reason() {
        let out = map_capture(FAILED_TURN);
        assert!(matches!(&out[..],
            [Payload::AttentionRequired(a)]
                if a.reason == AttentionReason::ErrorStalled
                    && a.message == "Model unavailable: zai-coding-plan/glm-4.5-air"));
    }

    #[test]
    fn text_ended_without_deltas_supplies_the_text_once() {
        let mut m = EventMapper::new();
        let out = m.on_event(&ev(
            "session.text.ended",
            json!({"sessionID": "s", "assistantMessageID": "msg_1", "ordinal": 0, "text": "whole"}),
        ));
        assert!(matches!(&out[..],
            [Payload::MessageStart(s), Payload::MessageDelta(d)]
                if s.message_id == "msg_1" && d.text == "whole"));
        let end = m.on_event(&ev(
            "session.step.ended",
            json!({"sessionID": "s", "assistantMessageID": "msg_1"}),
        ));
        assert!(matches!(&end[..], [Payload::MessageEnd(_)]));
    }

    #[test]
    fn tool_failure_and_interruption_map_to_errors() {
        let mut m = EventMapper::new();
        m.on_event(&ev(
            "session.tool.input.started",
            json!({"id": "c1", "name": "shell"}),
        ));
        let failed = m.on_event(&ev(
            "session.tool.failed",
            json!({"id": "c1", "error": {"type": "tool.denied", "message": "denied"}, "executed": false}),
        ));
        assert!(matches!(&failed[..],
            [Payload::ToolCall(t)] if t.status == ToolStatus::Error && t.name == "shell" && t.output == "denied"));
        let stopped = m.on_event(&ev(
            "session.execution.interrupted",
            json!({"sessionID": "s", "reason": "user"}),
        ));
        assert!(matches!(&stopped[..],
            [Payload::AttentionRequired(a)]
                if a.reason == AttentionReason::ErrorStalled && a.message == "interrupted (user)"));
    }

    #[test]
    fn permission_and_form_raise_attention() {
        let mut m = EventMapper::new();
        assert!(
            matches!(&m.on_event(&ev("permission.asked", json!({"id": "p", "action": "shell", "resources": []})))[..],
            [Payload::AttentionRequired(a)] if a.reason == AttentionReason::Permission)
        );
        assert!(matches!(&m.on_event(&ev("form.created", json!({})))[..],
            [Payload::AttentionRequired(a)] if a.reason == AttentionReason::NeedsInput));
    }

    #[test]
    fn opencode_1_envelopes_map_to_nothing() {
        let mut m = EventMapper::new();
        let v1 = json!({"type": "message.part.delta",
            "properties": {"messageID": "msg_a", "field": "text", "delta": "hi"}});
        assert!(m.on_event(&v1).is_empty());
        assert!(m
            .on_event(&json!({"type": "session.idle", "properties": {}}))
            .is_empty());
    }

    /// End-to-end over the real capture: raw SSE → `drain_sse` → the durable
    /// `SessionLog` that `session watch`/`subscribe` read.
    #[test]
    fn drain_sse_feeds_the_durable_log() {
        crate::test_util::with_isolated_home("opencode-drain-sse", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "ses-oc").expect("open log");
            let stop = AtomicBool::new(false);
            let n = drain_sse(Cursor::new(TOOL_TURN), "ses-oc", &mut log, &stop).expect("drain");
            assert_eq!(n, map_capture(TOOL_TURN).len());

            let events = SessionLog::open(&pb, "ses-oc")
                .unwrap()
                .read_from(0)
                .unwrap();
            assert_eq!(events.len(), n);
            assert_eq!(
                events.iter().map(|e| e.seq).collect::<Vec<_>>(),
                (1..=n as u64).collect::<Vec<_>>()
            );
            assert!(events
                .iter()
                .all(|e| e.actor == Some(crate::contract::Actor::agent("opencode"))));
        });
    }
}
