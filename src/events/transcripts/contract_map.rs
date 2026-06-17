//! Map a harness-agnostic [`TranscriptEvent`] onto the durable-log contract
//! ([`crate::contract::Payload`]). This is the translation the live tailer runs
//! to feed the per-session [`crate::events::log::SessionLog`] — the spine's
//! first real producer.
//!
//! The mapping is **lossless** against what a transcript exposes: a message
//! becomes the contract's `MessageStart`/`Delta`/`End` triple (one delta = the
//! complete text — the contract's vocabulary for "a message"), carrying the
//! model + stop reason on `MessageEnd`; token counts ride a correlated
//! [`Usage`] event (`source: native`); reasoning becomes a [`Thinking`] event;
//! a tool use + its result become the `Running` → `Completed`/`Error` pair of a
//! `ToolCall` sharing one `tool_call_id`.

use super::{EventKind, TranscriptEvent};
use crate::contract::{
    AttentionReason, AttentionRequired, MessageDelta, MessageEnd, MessageStart, Payload, Role,
    Thinking, ToolCall, ToolStatus, Usage, UsageSource,
};
use crate::events::otel::genai::GenAiUsage;

/// Translate one transcript event into the durable-log payloads it represents.
/// A text turn fans out to start/delta/end (+ a usage event when the harness
/// recorded counts); a tool use/result is a single `ToolCall`.
pub(super) fn to_payloads(event: &TranscriptEvent) -> Vec<Payload> {
    let id = event.uuid.as_str();
    match &event.kind {
        EventKind::UserPrompt { content } => message(id, Role::User, content, None, None),
        EventKind::AssistantText {
            text,
            model,
            usage,
            stop_reason,
        } => {
            let mut out = message(
                id,
                Role::Assistant,
                text,
                model.as_deref(),
                stop_reason.as_deref(),
            );
            if let Some(u) = usage {
                out.push(Payload::Usage(usage_payload(id, u)));
            }
            // Any terminal stop reason other than `tool_use` (which continues the
            // turn with a tool call) means the agent finished and awaits input —
            // the NeedsInput signal a front-end (orca / lum / Slack) flashes on and
            // `wait-idle` unblocks on. Must match `synth.rs`'s `turn_ended`:
            // `end_turn`, `stop_sequence`, and `max_tokens` all end the turn (a
            // missing reason is "not yet"). Keying only on `end_turn` would hang
            // `wait-idle` on a turn that stopped via a stop sequence or the token
            // cap. pillbox only *produces* the signal; the front-end surfaces it.
            // (The ambiguous mid-tool "blocked on permission" case — which a
            // quiescence timer can't tell from a slow tool — is the OSC/hook
            // channel's job, deferred.)
            if stop_reason.as_deref().is_some_and(|r| r != "tool_use") {
                out.push(Payload::AttentionRequired(AttentionRequired {
                    reason: AttentionReason::NeedsInput,
                    message: String::new(),
                }));
            }
            out
        }
        EventKind::AssistantThinking { text } => {
            vec![Payload::Thinking(Thinking { text: text.clone() })]
        }
        EventKind::ToolUse {
            tool_use_id,
            tool_name,
            input,
        } => vec![Payload::ToolCall(ToolCall {
            tool_call_id: tool_use_id.clone(),
            name: tool_name.clone(),
            status: ToolStatus::Running,
            input: Some(input.clone()),
            output: String::new(),
            title: String::new(),
        })],
        EventKind::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => vec![Payload::ToolCall(ToolCall {
            // The result correlates to the running call by id; `name` is empty
            // (the consumer already has it from the Running event).
            tool_call_id: tool_use_id.clone(),
            name: String::new(),
            status: if *is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Completed
            },
            input: None,
            output: content.clone(),
            title: String::new(),
        })],
    }
}

/// The start/delta/end triple for one complete message. `model`/`stop_reason`
/// ride `MessageEnd` (empty when absent — e.g. a user turn).
fn message(
    id: &str,
    role: Role,
    text: &str,
    model: Option<&str>,
    stop_reason: Option<&str>,
) -> Vec<Payload> {
    vec![
        Payload::MessageStart(MessageStart {
            message_id: id.to_string(),
            role,
        }),
        Payload::MessageDelta(MessageDelta {
            message_id: id.to_string(),
            text: text.to_string(),
        }),
        Payload::MessageEnd(MessageEnd {
            message_id: id.to_string(),
            model: model.unwrap_or_default().to_string(),
            stop_reason: stop_reason.unwrap_or_default().to_string(),
        }),
    ]
}

fn usage_payload(message_id: &str, u: &GenAiUsage) -> Usage {
    Usage {
        message_id: message_id.to_string(),
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        source: UsageSource::Native,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn event(kind: EventKind) -> TranscriptEvent {
        TranscriptEvent {
            uuid: "u1".into(),
            parent_uuid: None,
            timestamp: SystemTime::UNIX_EPOCH,
            kind,
        }
    }

    #[test]
    fn user_prompt_is_a_bare_message_triple() {
        let out = to_payloads(&event(EventKind::UserPrompt {
            content: "run the tests".into(),
        }));
        assert!(matches!(
            out.as_slice(),
            [
                Payload::MessageStart(s),
                Payload::MessageDelta(d),
                Payload::MessageEnd(e)
            ] if s.role == Role::User
                && s.message_id == "u1"
                && d.text == "run the tests"
                && e.model.is_empty()
                && e.stop_reason.is_empty()
        ));
    }

    #[test]
    fn assistant_text_carries_model_stop_reason_and_usage() {
        let usage = GenAiUsage {
            input_tokens: Some(100),
            output_tokens: Some(42),
            cache_read_input_tokens: Some(7),
            ..Default::default()
        };
        let out = to_payloads(&event(EventKind::AssistantText {
            text: "done".into(),
            model: Some("claude-opus-4-8".into()),
            usage: Some(usage),
            stop_reason: Some("end_turn".into()),
        }));
        // start, delta, end, usage, + the end_turn attention signal.
        assert_eq!(out.len(), 5);
        let Payload::MessageEnd(e) = &out[2] else {
            panic!("expected MessageEnd at [2]: {out:?}");
        };
        assert_eq!(e.model, "claude-opus-4-8");
        assert_eq!(e.stop_reason, "end_turn");
        let Payload::Usage(u) = &out[3] else {
            panic!("expected Usage at [3]: {out:?}");
        };
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.output_tokens, Some(42));
        assert_eq!(u.cache_read_input_tokens, Some(7));
        assert_eq!(u.source, UsageSource::Native);
        assert_eq!(u.message_id, "u1");
    }

    #[test]
    fn end_turn_produces_a_needs_input_attention_signal() {
        // `end_turn` = the agent finished and awaits input → the signal a
        // front-end flashes on. A non-terminal stop reason must NOT emit it.
        let ended = to_payloads(&event(EventKind::AssistantText {
            text: "done".into(),
            model: None,
            usage: None,
            stop_reason: Some("end_turn".into()),
        }));
        assert!(matches!(
            ended.last(),
            Some(Payload::AttentionRequired(a)) if a.reason == AttentionReason::NeedsInput
        ));

        let mid_turn = to_payloads(&event(EventKind::AssistantText {
            text: "let me run that".into(),
            model: None,
            usage: None,
            stop_reason: Some("tool_use".into()),
        }));
        assert!(
            !mid_turn
                .iter()
                .any(|p| matches!(p, Payload::AttentionRequired(_))),
            "no attention signal mid-turn (tool_use): {mid_turn:?}"
        );

        // A non-`end_turn` terminal reason (stop_sequence / max_tokens) ALSO ends
        // the turn and must emit the signal — else `wait-idle` hangs (the live
        // libkrun PTY smoke caught exactly this when claude stopped via a stop
        // sequence). Matches `synth.rs::turn_ended`.
        for terminal in ["stop_sequence", "max_tokens"] {
            let ended = to_payloads(&event(EventKind::AssistantText {
                text: "done".into(),
                model: None,
                usage: None,
                stop_reason: Some(terminal.into()),
            }));
            assert!(
                matches!(
                    ended.last(),
                    Some(Payload::AttentionRequired(a)) if a.reason == AttentionReason::NeedsInput
                ),
                "terminal stop_reason `{terminal}` must emit NeedsInput: {ended:?}"
            );
        }
    }

    #[test]
    fn assistant_text_without_usage_omits_the_usage_event() {
        let out = to_payloads(&event(EventKind::AssistantText {
            text: "hi".into(),
            model: None,
            usage: None,
            stop_reason: None,
        }));
        assert_eq!(
            out.len(),
            3,
            "no usage event when the harness didn't record counts"
        );
    }

    #[test]
    fn thinking_maps_to_a_thinking_event() {
        let out = to_payloads(&event(EventKind::AssistantThinking {
            text: "let me reason".into(),
        }));
        assert!(matches!(out.as_slice(), [Payload::Thinking(t)] if t.text == "let me reason"));
    }

    #[test]
    fn tool_use_and_result_share_id_across_running_then_terminal() {
        let used = to_payloads(&event(EventKind::ToolUse {
            tool_use_id: "tc-9".into(),
            tool_name: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }));
        assert!(matches!(
            used.as_slice(),
            [Payload::ToolCall(t)] if t.tool_call_id == "tc-9"
                && t.name == "Bash"
                && t.status == ToolStatus::Running
                && t.input.is_some()
        ));

        let errored = to_payloads(&event(EventKind::ToolResult {
            tool_use_id: "tc-9".into(),
            content: "boom".into(),
            is_error: true,
        }));
        assert!(matches!(
            errored.as_slice(),
            [Payload::ToolCall(t)] if t.tool_call_id == "tc-9"
                && t.status == ToolStatus::Error
                && t.output == "boom"
                && t.input.is_none()
        ));
    }
}
