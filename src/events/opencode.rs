//! Map opencode's `/event` SSE envelopes into pillbox §0 `contract::Payload`s.
//!
//! opencode is **structured-API-native**: `opencode serve` exposes a headless
//! HTTP server whose `/event` endpoint streams typed events, so instead of
//! scraping a transcript file (the claude/codex `transcripts` path) we consume
//! its event stream directly — structured, real-time, no file-tailing. This
//! module is the pure mapping core; the SSE transport + the bridge that feeds
//! the durable [`SessionLog`](crate::events::log::SessionLog) live in the
//! sandbox run path. See `docs` / the opencode OpenAPI (`opencode serve` →
//! `GET /doc`).
//!
//! Each opencode event is `{ "type": "<dotted>", "properties": {...} }`. We map
//! the `session.next.*` streaming family (the assistant turn) plus the
//! attention signals (`session.idle` / `permission.asked` / `question.asked`)
//! and ignore everything else — crucially the parallel `message.*` / `Part`
//! family, which carries the *same* content in a persisted-message shape and
//! would double-count if mapped alongside the streaming deltas.
//!
//! Stateful: the streaming text events carry only a `sessionID` (no message
//! id), so we synthesize a per-turn message id (bumped on `text.started`) to
//! correlate start/delta/end, and remember each tool's `callID → name` so the
//! later success/failed event can name the tool it completed.
//!
//! The pure mapper lands first (fully unit-tested); the SSE transport + the
//! `serve`-mode run wiring that feeds the durable log are the next slice, at
//! which point these become live.
#![allow(dead_code)]

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{
    AttentionReason, AttentionRequired, MessageDelta, MessageEnd, MessageStart, Payload, Role,
    Thinking, ToolCall, ToolStatus,
};

/// Stateful opencode-event → §0-payload mapper. One per session stream.
#[derive(Default)]
pub(crate) struct EventMapper {
    /// Bumped on each `text.started`; combined with the session id to form a
    /// stable per-turn `message_id` the start/delta/end share.
    turn: u64,
    /// The current assistant message id (set on `text.started`, cleared on
    /// `text.ended`) so deltas correlate without their own id.
    msg_id: Option<String>,
    /// `callID → tool name`, so `tool.success`/`tool.failed` (which omit the
    /// name) can re-attach it to the completed call.
    tools: HashMap<String, String>,
}

impl EventMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Map one opencode `/event` envelope into zero or more §0 payloads.
    /// Unknown / unmapped types return an empty vec (the stream carries far
    /// more than the assistant turn — lifecycle, sync, tui, lsp, …).
    pub(crate) fn on_event(&mut self, ev: &Value) -> Vec<Payload> {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or_default();
        let p = ev.get("properties").unwrap_or(&Value::Null);
        let str_of = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };

        match ty {
            "session.next.text.started" => {
                self.turn += 1;
                let id = format!("{}:{}", str_of("sessionID"), self.turn);
                self.msg_id = Some(id.clone());
                vec![Payload::MessageStart(MessageStart {
                    message_id: id,
                    role: Role::Assistant,
                })]
            }
            "session.next.text.delta" => {
                let text = str_of("delta");
                if text.is_empty() {
                    return vec![];
                }
                vec![Payload::MessageDelta(MessageDelta {
                    message_id: self.current_msg_id(p),
                    text,
                })]
            }
            "session.next.text.ended" => {
                let id = self.msg_id.take().unwrap_or_else(|| str_of("sessionID"));
                vec![Payload::MessageEnd(MessageEnd::new(id))]
            }
            "session.next.reasoning.delta" => {
                let text = str_of("delta");
                if text.is_empty() {
                    return vec![];
                }
                vec![Payload::Thinking(Thinking { text })]
            }
            "session.next.tool.called" => {
                let call_id = str_of("callID");
                let name = str_of("tool");
                self.tools.insert(call_id.clone(), name.clone());
                vec![Payload::ToolCall(ToolCall {
                    tool_call_id: call_id,
                    name,
                    status: ToolStatus::Running,
                    input: p.get("input").cloned(),
                    output: String::new(),
                    title: String::new(),
                })]
            }
            "session.next.tool.success" => {
                let call_id = str_of("callID");
                vec![Payload::ToolCall(ToolCall {
                    name: self.tools.remove(&call_id).unwrap_or_default(),
                    tool_call_id: call_id,
                    status: ToolStatus::Completed,
                    input: None,
                    output: tool_output(p),
                    title: String::new(),
                })]
            }
            "session.next.tool.failed" => {
                let call_id = str_of("callID");
                vec![Payload::ToolCall(ToolCall {
                    name: self.tools.remove(&call_id).unwrap_or_default(),
                    tool_call_id: call_id,
                    status: ToolStatus::Error,
                    input: None,
                    output: str_of("error"),
                    title: String::new(),
                })]
            }
            // Turn went quiescent → the agent is waiting on the driver. Same
            // attention signal the claude transcript producer emits on end_turn.
            "session.idle" => vec![attention(AttentionReason::NeedsInput)],
            "permission.asked" => vec![attention(AttentionReason::Permission)],
            "question.asked" => vec![attention(AttentionReason::NeedsInput)],
            "session.error" => vec![Payload::AttentionRequired(AttentionRequired {
                reason: AttentionReason::ErrorStalled,
                message: error_message(p),
            })],
            _ => vec![],
        }
    }

    /// The active assistant message id for a delta — the one opened by
    /// `text.started`, falling back to the bare session id if a delta somehow
    /// arrives before a start (defensive; keeps deltas from being dropped).
    fn current_msg_id(&self, props: &Value) -> String {
        self.msg_id.clone().unwrap_or_else(|| {
            props
                .get("sessionID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
    }
}

fn attention(reason: AttentionReason) -> Payload {
    Payload::AttentionRequired(AttentionRequired {
        reason,
        message: String::new(),
    })
}

/// Best-effort human-readable output from a `tool.success` event: prefer a
/// string `content`, else compact-JSON the `structured` result.
fn tool_output(props: &Value) -> String {
    match props.get("content") {
        Some(Value::String(s)) => s.clone(),
        _ => props
            .get("structured")
            .map(|v| v.to_string())
            .unwrap_or_default(),
    }
}

/// Pull a message out of a `session.error` event's `error`, which is either a
/// string or an object with a `message`/`data.message`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(ty: &str, props: Value) -> Value {
        json!({ "id": "evt_x", "type": ty, "properties": props })
    }

    #[test]
    fn streaming_text_turn_maps_to_start_delta_end_with_one_id() {
        let mut m = EventMapper::new();
        let started = m.on_event(&ev(
            "session.next.text.started",
            json!({ "timestamp": 1.0, "sessionID": "ses_abc" }),
        ));
        let id = match &started[..] {
            [Payload::MessageStart(s)] => {
                assert_eq!(s.role, Role::Assistant);
                s.message_id.clone()
            }
            other => panic!("expected MessageStart, got {other:?}"),
        };
        assert_eq!(id, "ses_abc:1");

        let d = m.on_event(&ev(
            "session.next.text.delta",
            json!({ "timestamp": 2.0, "sessionID": "ses_abc", "delta": "hello" }),
        ));
        assert!(
            matches!(&d[..], [Payload::MessageDelta(x)] if x.text == "hello" && x.message_id == id)
        );

        let e = m.on_event(&ev(
            "session.next.text.ended",
            json!({ "timestamp": 3.0, "sessionID": "ses_abc", "text": "hello" }),
        ));
        assert!(matches!(&e[..], [Payload::MessageEnd(x)] if x.message_id == id));

        // A second turn gets a fresh id, not a collision with the first.
        let started2 = m.on_event(&ev(
            "session.next.text.started",
            json!({ "sessionID": "ses_abc" }),
        ));
        assert!(matches!(&started2[..], [Payload::MessageStart(s)] if s.message_id == "ses_abc:2"));
    }

    #[test]
    fn reasoning_delta_maps_to_thinking_and_empty_deltas_drop() {
        let mut m = EventMapper::new();
        let t = m.on_event(&ev(
            "session.next.reasoning.delta",
            json!({ "sessionID": "ses_a", "reasoningID": "r1", "delta": "let me think" }),
        ));
        assert!(matches!(&t[..], [Payload::Thinking(x)] if x.text == "let me think"));
        // Empty deltas produce nothing (no noise events in the log).
        assert!(m
            .on_event(&ev(
                "session.next.text.delta",
                json!({ "sessionID": "ses_a", "delta": "" })
            ))
            .is_empty());
    }

    #[test]
    fn tool_call_running_then_success_carries_the_name_forward() {
        let mut m = EventMapper::new();
        let called = m.on_event(&ev(
            "session.next.tool.called",
            json!({ "sessionID": "ses_a", "callID": "c1", "tool": "bash",
                    "input": { "command": "ls" }, "provider": { "executed": true } }),
        ));
        assert!(matches!(&called[..], [Payload::ToolCall(t)]
            if t.tool_call_id == "c1" && t.name == "bash" && t.status == ToolStatus::Running
               && t.input.as_ref().and_then(|i| i.get("command")).and_then(|c| c.as_str()) == Some("ls")));

        // success omits the tool name; the mapper re-attaches it via callID.
        let ok = m.on_event(&ev(
            "session.next.tool.success",
            json!({ "sessionID": "ses_a", "callID": "c1", "content": "a\nb",
                    "provider": { "executed": true } }),
        ));
        assert!(matches!(&ok[..], [Payload::ToolCall(t)]
            if t.tool_call_id == "c1" && t.name == "bash"
               && t.status == ToolStatus::Completed && t.output == "a\nb"));
    }

    #[test]
    fn tool_failed_maps_to_error_status() {
        let mut m = EventMapper::new();
        m.on_event(&ev(
            "session.next.tool.called",
            json!({ "sessionID": "s", "callID": "c9", "tool": "edit", "input": {} }),
        ));
        let f = m.on_event(&ev(
            "session.next.tool.failed",
            json!({ "sessionID": "s", "callID": "c9", "error": "file not found" }),
        ));
        assert!(matches!(&f[..], [Payload::ToolCall(t)]
            if t.status == ToolStatus::Error && t.name == "edit" && t.output == "file not found"));
    }

    #[test]
    fn idle_and_permission_and_question_map_to_attention() {
        let mut m = EventMapper::new();
        assert!(
            matches!(&m.on_event(&ev("session.idle", json!({ "sessionID": "s" })))[..],
            [Payload::AttentionRequired(a)] if a.reason == AttentionReason::NeedsInput)
        );
        assert!(
            matches!(&m.on_event(&ev("permission.asked", json!({})))[..],
            [Payload::AttentionRequired(a)] if a.reason == AttentionReason::Permission)
        );
        assert!(matches!(&m.on_event(&ev("question.asked", json!({})))[..],
            [Payload::AttentionRequired(a)] if a.reason == AttentionReason::NeedsInput));
    }

    #[test]
    fn unmapped_and_duplicate_families_are_ignored() {
        let mut m = EventMapper::new();
        // server lifecycle, the persisted message.* family, sync.*, tui.* — all
        // ignored so the streaming session.next.* family isn't double-counted.
        for ty in [
            "server.connected",
            "session.created",
            "message.updated",
            "message.part.updated",
            "sync.event.session.next.text.delta",
            "tui.toast.show",
        ] {
            assert!(
                m.on_event(&ev(ty, json!({ "sessionID": "s" }))).is_empty(),
                "{ty} should map to nothing"
            );
        }
    }
}
