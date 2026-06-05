//! Map `codex app-server`'s JSON-RPC notifications into pillbox §0
//! [`contract::Payload`]s — the codex sibling of [`crate::events::opencode`].
//!
//! `codex app-server` (the channel the Codex VS Code extension drives) speaks
//! **JSON-RPC 2.0 with the `"jsonrpc":"2.0"` header omitted on the wire**, as
//! newline-delimited JSON over stdio. The in-guest [`appserver-host`] bridge
//! (`crate::sandbox::appserver`) owns that stdio pipe, does the `initialize` /
//! `thread/start` handshake, and appends each **notification** line verbatim to
//! a capture file. This module is the pure mapping core the host drains that
//! file through; the NDJSON transport + the bridge live in the run path.
//!
//! ## The notification envelope
//!
//! A notification is `{"method":"<resource>/<verb>","params":{…}}` (no `id`).
//! The turn streams over these (verified shapes from `codex app-server
//! generate-json-schema` at codex 0.137.0):
//!
//! - `turn/started` → `{threadId, turn}` — a turn began (→ `Thinking`).
//! - `item/agentMessage/delta` → `{itemId, delta, threadId, turnId}` — assistant
//!   text. First delta for an `itemId` opens the message; subsequent deltas
//!   append.
//! - `item/reasoning/textDelta` / `item/reasoning/summaryTextDelta` →
//!   `{itemId, delta, …}` — reasoning (→ `Thinking`).
//! - `item/started` / `item/completed` → `{item, threadId, turnId, …}` — a
//!   `ThreadItem` (agentMessage / commandExecution / fileChange / mcpToolCall /
//!   …). Tool-shaped items become a `ToolCall`; the agentMessage item closes the
//!   open message.
//! - `turn/completed` → `{threadId, turn}` — the turn went idle (close the open
//!   message + raise the attention signal; `turn.status == "failed"` → stalled).
//! - `error` → `{error, threadId, turnId, willRetry}` — a turn-level error.
//!
//! Everything else (account/*, thread lifecycle, mcp startup, fuzzyFileSearch,
//! deltas for output we don't surface) is ignored — the stream carries far more
//! than the turn.
//!
//! ## Stateful normalization
//!
//! `item/agentMessage/delta` carries no role and no explicit open/close, so we
//! track the currently-open assistant message id and emit one `MessageStart` on
//! its first delta, closing it on the matching `item/completed` or at
//! `turn/completed`. A tool item's status is emitted only when it *changes*
//! (`item/started` Running → `item/completed` Completed/Error) so the input
//! stream doesn't flood the log with duplicate `ToolCall`s.

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{
    AgentPhase, AttentionReason, AttentionRequired, Event, MessageDelta, MessageEnd, MessageStart,
    Payload, PhaseChanged, Role, RunStarted, Thinking, ToolCall, ToolStatus,
};
use crate::events::log::SessionLog;

/// Stateful codex-app-server-notification → §0-payload mapper. One per session
/// stream (the capture file is drained start-to-finish by a single mapper).
#[derive(Default)]
pub(crate) struct CodexServeMapper {
    /// itemId of the currently-open assistant message (set on its first
    /// `item/agentMessage/delta`, cleared when that item completes or the turn
    /// ends). codex streams one agentMessage item per turn, so comparing against
    /// this suppresses duplicate `MessageStart`s without an unbounded seen-set.
    open_msg: Option<String>,
    /// `itemId → last emitted tool status`, so a `ToolCall` is emitted only when
    /// a tool item's status actually changes (started→completed), not on every
    /// re-delivery. Keyed on the mapped status.
    tool_status: HashMap<String, ToolStatus>,
}

impl CodexServeMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Map one codex app-server **notification** (`{method, params}`) into zero
    /// or more §0 payloads. Unmapped methods (and any responses/requests that
    /// slip in) return empty.
    pub(crate) fn on_notification(&mut self, msg: &Value) -> Vec<Payload> {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let p = msg.get("params").unwrap_or(&Value::Null);
        match method {
            "thread/started" => vec![Payload::RunStarted(RunStarted {
                agent: "codex".into(),
                parent_run_id: String::new(),
                base_snapshot: String::new(),
            })],
            "turn/started" => vec![Payload::PhaseChanged(PhaseChanged {
                phase: AgentPhase::Thinking,
            })],
            "item/agentMessage/delta" => self.on_agent_delta(p),
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => on_reasoning(p),
            "item/started" => self.on_item(p, false),
            "item/completed" => self.on_item(p, true),
            "turn/completed" => self.on_turn_completed(p),
            "error" => vec![Payload::AttentionRequired(AttentionRequired {
                reason: AttentionReason::ErrorStalled,
                message: turn_error_message(p.get("error")),
            })],
            _ => vec![],
        }
    }

    /// `item/agentMessage/delta` — open the assistant message on the first delta
    /// for its `itemId`, then append. Empty deltas drop.
    fn on_agent_delta(&mut self, p: &Value) -> Vec<Payload> {
        let item_id = str_field(p, "itemId");
        let delta = str_field(p, "delta");
        if item_id.is_empty() || delta.is_empty() {
            return vec![];
        }
        let mut out = Vec::new();
        if self.open_msg.as_deref() != Some(item_id) {
            self.open_msg = Some(item_id.to_string());
            out.push(Payload::MessageStart(MessageStart {
                message_id: item_id.to_string(),
                role: Role::Assistant,
            }));
        }
        out.push(Payload::MessageDelta(MessageDelta {
            message_id: item_id.to_string(),
            text: delta.to_string(),
        }));
        out
    }

    /// `item/started` (`completed = false`) or `item/completed` (`true`) — map
    /// the carried [`ThreadItem`] by its `type`. agentMessage items close the
    /// open message; the tool-shaped items become a `ToolCall`; user/reasoning/
    /// boundary items are handled elsewhere or ignored.
    fn on_item(&mut self, p: &Value, completed: bool) -> Vec<Payload> {
        let item = p.get("item").unwrap_or(&Value::Null);
        let item_type = str_field(item, "type");
        match item_type {
            // The assistant message: deltas already streamed the text, so
            // `started` is a no-op and `completed` just closes the open message.
            "agentMessage" => {
                if !completed {
                    return vec![];
                }
                let id = str_field(item, "id");
                // Close whichever message is open (the completed item's id, or a
                // delta-opened one). If no delta ever arrived (a whole-message
                // item with no streaming), synthesize start+delta from `text`.
                let mut out = Vec::new();
                if self.open_msg.is_none() && !id.is_empty() {
                    let text = str_field(item, "text");
                    out.push(Payload::MessageStart(MessageStart {
                        message_id: id.to_string(),
                        role: Role::Assistant,
                    }));
                    if !text.is_empty() {
                        out.push(Payload::MessageDelta(MessageDelta {
                            message_id: id.to_string(),
                            text: text.to_string(),
                        }));
                    }
                }
                if let Some(open) = self.open_msg.take() {
                    out.push(Payload::MessageEnd(MessageEnd::new(open)));
                } else if !id.is_empty() {
                    out.push(Payload::MessageEnd(MessageEnd::new(id)));
                }
                out
            }
            "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "webSearch"
            | "collabAgentToolCall" => self.on_tool_item(item, item_type, completed),
            // userMessage (our own prompt echo), reasoning (deltas drive it),
            // plan, review-mode, contextCompaction, image* — nothing to surface.
            _ => vec![],
        }
    }

    /// A tool-shaped [`ThreadItem`] → a `ToolCall`, de-duplicated by item id +
    /// mapped status (so `item/started`'s Running and `item/completed`'s
    /// terminal status each emit once, re-deliveries none).
    fn on_tool_item(&mut self, item: &Value, item_type: &str, completed: bool) -> Vec<Payload> {
        let id = str_field(item, "id").to_string();
        if id.is_empty() {
            return vec![];
        }
        // `item/completed` carries the item's terminal `status`
        // (completed/failed); `item/started` has no terminal status yet → Running.
        let status = if completed {
            map_item_status(str_field(item, "status"))
        } else {
            ToolStatus::Running
        };
        if self.tool_status.get(&id) == Some(&status) {
            return vec![];
        }
        self.tool_status.insert(id.clone(), status);
        vec![Payload::ToolCall(ToolCall {
            tool_call_id: id,
            name: tool_name(item, item_type),
            status,
            input: tool_input(item, item_type),
            output: if completed {
                tool_output(item, item_type)
            } else {
                String::new()
            },
            title: String::new(),
        })]
    }

    /// `turn/completed` — the turn went idle. Close any open assistant message
    /// and raise the attention signal `wait-idle` keys on. A failed/interrupted
    /// turn raises `ErrorStalled` instead of `NeedsInput`.
    fn on_turn_completed(&mut self, p: &Value) -> Vec<Payload> {
        let mut out = Vec::new();
        if let Some(open) = self.open_msg.take() {
            out.push(Payload::MessageEnd(MessageEnd::new(open)));
        }
        let status = p
            .get("turn")
            .map(|t| str_field(t, "status"))
            .unwrap_or_default();
        let reason = match status {
            "failed" | "interrupted" => AttentionReason::ErrorStalled,
            _ => AttentionReason::NeedsInput,
        };
        out.push(Payload::AttentionRequired(AttentionRequired {
            reason,
            message: String::new(),
        }));
        out
    }
}

/// Map a reasoning text delta (`item/reasoning/{textDelta,summaryTextDelta}`) to
/// a `Thinking` payload. Empty deltas drop.
fn on_reasoning(p: &Value) -> Vec<Payload> {
    let delta = str_field(p, "delta");
    if delta.is_empty() {
        return vec![];
    }
    vec![Payload::Thinking(Thinking {
        text: delta.to_string(),
    })]
}

/// codex `ThreadItem.status` → §0 `ToolStatus`. Only `item/completed` carries a
/// terminal status; `completed` is success, `failed` an error, anything else
/// (still in flight) stays Running.
fn map_item_status(s: &str) -> ToolStatus {
    match s {
        "completed" => ToolStatus::Completed,
        "failed" => ToolStatus::Error,
        _ => ToolStatus::Running,
    }
}

/// A display name for a tool item. commandExecution/fileChange have no name
/// field — use the kind; mcp/dynamic tool calls carry a `tool`/`name`.
fn tool_name(item: &Value, item_type: &str) -> String {
    match item_type {
        "mcpToolCall" | "dynamicToolCall" => {
            let name = str_field(item, "tool");
            let name = if name.is_empty() {
                str_field(item, "name")
            } else {
                name
            };
            if name.is_empty() {
                item_type.to_string()
            } else {
                name.to_string()
            }
        }
        _ => item_type.to_string(),
    }
}

/// The tool item's structured input, when the item shape carries one (the
/// command for an exec, the changes for a file edit, the args for an mcp call).
fn tool_input(item: &Value, item_type: &str) -> Option<Value> {
    match item_type {
        "commandExecution" => item.get("command").filter(|v| !v.is_null()).cloned(),
        "fileChange" => item.get("changes").filter(|v| !v.is_null()).cloned(),
        "mcpToolCall" | "dynamicToolCall" => item
            .get("arguments")
            .or_else(|| item.get("input"))
            .filter(|v| !v.is_null())
            .cloned(),
        "webSearch" => item.get("query").filter(|v| !v.is_null()).cloned(),
        _ => None,
    }
}

/// The tool item's output text at completion (the captured command output, the
/// mcp result). Best-effort: returns "" when the shape carries nothing textual.
fn tool_output(item: &Value, item_type: &str) -> String {
    match item_type {
        "commandExecution" => str_field(item, "aggregatedOutput").to_string(),
        _ => match item.get("result").or_else(|| item.get("output")) {
            Some(Value::String(s)) => s.clone(),
            Some(other @ Value::Object(_)) | Some(other @ Value::Array(_)) => other.to_string(),
            _ => String::new(),
        },
    }
}

/// Flatten a codex `TurnError` (`{message}` or a bare string) to a display line.
fn turn_error_message(error: Option<&Value>) -> String {
    match error {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string(),
        _ => "error".to_string(),
    }
}

/// Borrow a string field as `&str`, or `""`.
fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Drain a codex app-server **NDJSON** capture (one JSON message per line) into
/// the durable [`SessionLog`], mapping each notification through
/// [`CodexServeMapper`] — the codex analog of [`drain_sse`](crate::events::opencode::drain_sse),
/// minus the SSE framing (codex's wire is already line-delimited JSON, so a line
/// *is* a message).
///
/// The in-guest [`appserver-host`](crate::sandbox::appserver) bridge appends
/// each notification line to the capture file; the host drains it (replay +
/// follow) on `watch`/`subscribe`/`ingest`, exactly like opencode's `/event`
/// file. Pass a [`FollowReader`](crate::events::opencode::FollowReader) for a
/// live session (tails appends) or a plain `File` for a post-hoc drain (reads to
/// EOF). Blocks until the reader ends or `stop` is set (observed between lines).
/// Non-JSON lines and unmapped methods are skipped, not errored, so a stray line
/// can't wedge the drain. Returns the number of §0 events appended.
pub(crate) fn drain_ndjson<R: std::io::Read>(
    reader: R,
    session_id: &str,
    log: &mut SessionLog,
    stop: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<usize> {
    use std::io::BufRead as _;
    use std::sync::atomic::Ordering;

    let mut mapper = CodexServeMapper::new();
    let mut total = 0;
    let mut lines = std::io::BufReader::new(reader).lines();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(line) = lines.next() else { break };
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let events: Vec<Event> = mapper
            .on_notification(&value)
            .into_iter()
            .map(|p| Event::session(session_id, p))
            .collect();
        if !events.is_empty() {
            total += events.len();
            log.append(&events)?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Fixtures are the real codex app-server notification shapes
    // (`{method, params}`, `"jsonrpc"` omitted on the wire), taken from
    // `codex app-server generate-json-schema` at codex 0.137.0.
    fn run(msgs: &[Value]) -> Vec<Payload> {
        let mut m = CodexServeMapper::new();
        msgs.iter().flat_map(|e| m.on_notification(e)).collect()
    }

    #[test]
    fn thread_started_maps_to_run_started() {
        let out = run(&[json!({
            "method":"thread/started",
            "params":{"thread":{"id":"th_1","status":"idle"}}
        })]);
        assert!(matches!(out.as_slice(), [Payload::RunStarted(r)] if r.agent == "codex"));
    }

    #[test]
    fn turn_started_maps_to_thinking_phase() {
        let out = run(&[json!({
            "method":"turn/started",
            "params":{"threadId":"th_1","turn":{"id":"tu_1","status":"inProgress","items":[]}}
        })]);
        assert!(
            matches!(out.as_slice(), [Payload::PhaseChanged(p)] if p.phase == AgentPhase::Thinking)
        );
    }

    #[test]
    fn agent_message_delta_opens_once_then_appends() {
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_msg","delta":"Hel","threadId":"th","turnId":"tu"}}),
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_msg","delta":"lo","threadId":"th","turnId":"tu"}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(s), Payload::MessageDelta(d1), Payload::MessageDelta(d2)] => {
                assert_eq!(s.role, Role::Assistant);
                assert_eq!(s.message_id, "it_msg");
                assert_eq!(d1.text, "Hel");
                assert_eq!(d2.text, "lo");
                assert_eq!(d2.message_id, "it_msg");
            }
            other => panic!("expected start + two deltas, got {other:?}"),
        }
    }

    #[test]
    fn agent_message_completed_closes_the_open_message() {
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_msg","delta":"hi","threadId":"th","turnId":"tu"}}),
            json!({"method":"item/completed","params":{
                "threadId":"th","turnId":"tu","completedAtMs":1,
                "item":{"type":"agentMessage","id":"it_msg","text":"hi"}}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(_), Payload::MessageDelta(_), Payload::MessageEnd(e)] => {
                assert_eq!(e.message_id, "it_msg");
            }
            other => panic!("expected start/delta/end, got {other:?}"),
        }
    }

    #[test]
    fn agent_message_item_without_deltas_synthesizes_whole_message() {
        // A completed agentMessage with no preceding deltas (non-streaming path):
        // must still surface start + the full text + end.
        let out = run(&[json!({"method":"item/completed","params":{
            "threadId":"th","turnId":"tu","completedAtMs":1,
            "item":{"type":"agentMessage","id":"it_x","text":"the whole reply"}}})]);
        match out.as_slice() {
            [Payload::MessageStart(s), Payload::MessageDelta(d), Payload::MessageEnd(e)] => {
                assert_eq!(s.message_id, "it_x");
                assert_eq!(d.text, "the whole reply");
                assert_eq!(e.message_id, "it_x");
            }
            other => panic!("expected start/delta/end, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_delta_maps_to_thinking() {
        let out = run(&[json!({"method":"item/reasoning/textDelta","params":{
            "itemId":"it_r","contentIndex":0,"delta":"let me think","threadId":"th","turnId":"tu"}})]);
        assert!(matches!(out.as_slice(), [Payload::Thinking(t)] if t.text == "let me think"));
    }

    #[test]
    fn command_execution_started_then_completed_pairs_by_id() {
        let out = run(&[
            json!({"method":"item/started","params":{
                "threadId":"th","turnId":"tu","startedAtMs":1,
                "item":{"type":"commandExecution","id":"it_c","command":"echo HELLO",
                        "commandActions":[],"cwd":"/workspace","status":"inProgress"}}}),
            json!({"method":"item/completed","params":{
                "threadId":"th","turnId":"tu","completedAtMs":2,
                "item":{"type":"commandExecution","id":"it_c","command":"echo HELLO",
                        "commandActions":[],"cwd":"/workspace","status":"completed",
                        "exitCode":0,"aggregatedOutput":"HELLO\n"}}}),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(running), Payload::ToolCall(done)] => {
                assert_eq!(running.tool_call_id, "it_c");
                assert_eq!(running.name, "commandExecution");
                assert_eq!(running.status, ToolStatus::Running);
                assert_eq!(running.input.as_ref().unwrap(), "echo HELLO");
                assert_eq!(done.status, ToolStatus::Completed);
                assert_eq!(done.output, "HELLO\n");
            }
            other => panic!("expected two ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn command_execution_failed_maps_to_error_status() {
        let out = run(&[json!({"method":"item/completed","params":{
            "threadId":"th","turnId":"tu","completedAtMs":2,
            "item":{"type":"commandExecution","id":"it_f","command":"false",
                    "commandActions":[],"cwd":"/w","status":"failed","exitCode":1,
                    "aggregatedOutput":"boom"}}})]);
        match out.as_slice() {
            [Payload::ToolCall(t)] => {
                assert_eq!(t.status, ToolStatus::Error);
                assert_eq!(t.output, "boom");
            }
            other => panic!("expected one ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_item_same_status_redelivered_is_deduped() {
        let out = run(&[
            json!({"method":"item/started","params":{"threadId":"t","turnId":"u","startedAtMs":1,
                "item":{"type":"commandExecution","id":"c","command":"x","commandActions":[],
                        "cwd":"/w","status":"inProgress"}}}),
            json!({"method":"item/started","params":{"threadId":"t","turnId":"u","startedAtMs":1,
                "item":{"type":"commandExecution","id":"c","command":"x","commandActions":[],
                        "cwd":"/w","status":"inProgress"}}}),
        ]);
        assert_eq!(out.len(), 1, "running re-delivered → one event");
    }

    #[test]
    fn mcp_tool_call_uses_tool_name() {
        let out = run(&[json!({"method":"item/completed","params":{
            "threadId":"t","turnId":"u","completedAtMs":2,
            "item":{"type":"mcpToolCall","id":"m1","tool":"search","status":"completed",
                    "arguments":{"q":"rust"},"result":"hit"}}})]);
        match out.as_slice() {
            [Payload::ToolCall(t)] => {
                assert_eq!(t.name, "search");
                assert_eq!(t.input.as_ref().unwrap()["q"], "rust");
                assert_eq!(t.output, "hit");
            }
            other => panic!("expected one ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn turn_completed_closes_message_and_signals_idle() {
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_m","delta":"done","threadId":"th","turnId":"tu"}}),
            json!({"method":"turn/completed","params":{
                "threadId":"th","turn":{"id":"tu","status":"completed","items":[]}}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(_), Payload::MessageDelta(_), Payload::MessageEnd(e), Payload::AttentionRequired(a)] =>
            {
                assert_eq!(e.message_id, "it_m");
                assert_eq!(a.reason, AttentionReason::NeedsInput);
            }
            other => panic!("expected start/delta/end + idle attention, got {other:?}"),
        }
    }

    #[test]
    fn turn_completed_failed_raises_error_stalled() {
        let out = run(&[json!({"method":"turn/completed","params":{
            "threadId":"th","turn":{"id":"tu","status":"failed","items":[]}}})]);
        assert!(matches!(
            out.as_slice(),
            [Payload::AttentionRequired(a)] if a.reason == AttentionReason::ErrorStalled
        ));
    }

    #[test]
    fn error_notification_becomes_attention_with_message() {
        let out = run(&[json!({"method":"error","params":{
            "threadId":"th","turnId":"tu","willRetry":false,
            "error":{"message":"model overloaded"}}})]);
        match out.as_slice() {
            [Payload::AttentionRequired(a)] => {
                assert_eq!(a.reason, AttentionReason::ErrorStalled);
                assert_eq!(a.message, "model overloaded");
            }
            other => panic!("expected AttentionRequired, got {other:?}"),
        }
    }

    #[test]
    fn drain_ndjson_feeds_the_durable_log() {
        use std::io::Cursor;
        use std::sync::atomic::AtomicBool;

        crate::test_util::with_isolated_home("codex-serve-drain", || {
            let g = crate::pillbox::global();
            let mut log = SessionLog::open(&g, "ses-cx").expect("open log");
            // A minimal turn: thread start, one streamed message, idle.
            let stream = [
                r#"{"method":"thread/started","params":{"thread":{"id":"th","status":"idle"}}}"#,
                r#"{"method":"item/agentMessage/delta","params":{"itemId":"m","delta":"hi","threadId":"th","turnId":"tu"}}"#,
                r#"{"id":1,"result":{"turn":{"id":"tu"}}}"#,
                r#"not json — must be skipped, not fatal"#,
                r#"{"method":"turn/completed","params":{"threadId":"th","turn":{"id":"tu","status":"completed","items":[]}}}"#,
            ]
            .join("\n");
            let stop = AtomicBool::new(false);
            let n = drain_ndjson(Cursor::new(stream), "ses-cx", &mut log, &stop).expect("drain");
            // RunStarted, MessageStart, MessageDelta, MessageEnd, AttentionRequired = 5.
            assert_eq!(n, 5, "expected 5 mapped §0 events");
        });
    }

    #[test]
    fn unknown_and_lifecycle_methods_are_ignored() {
        assert!(run(&[json!({"method":"account/updated","params":{}})]).is_empty());
        assert!(run(&[json!({"method":"thread/tokenUsage/updated","params":{}})]).is_empty());
        assert!(run(&[json!({"method":"mcpServer/startupStatus/updated","params":{}})]).is_empty());
        // A response (has `id`+`result`, no `method`) must not map to anything.
        assert!(run(&[json!({"id":7,"result":{"turn":{"id":"tu"}}})]).is_empty());
    }
}
