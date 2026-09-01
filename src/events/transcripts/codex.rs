//! Codex (OpenAI CLI) transcript JSONL parser.
//!
//! Codex writes one JSONL file per session at
//! `~/.codex/sessions/<year>/<month>/<day>/rollout-<id>.jsonl`. Each
//! line has a `type`-tagged envelope; the meaty content sits under
//! `type: "response_item"` where `payload.type` discriminates:
//!
//! - `message` (role=user / assistant) → user prompt or assistant
//!   text. Codex uses OpenAI's `input_text` / `output_text` content
//!   blocks; we concatenate their `.text` fields into one string
//!   per message.
//! - `function_call` → tool invocation (name, JSON-string
//!   arguments, call_id).
//! - `function_call_output` → tool result (call_id, output as a
//!   single string).
//! - `reasoning` → assistant thinking trace.
//!
//! Envelope-only types (`session_meta`, `turn_context`, `event_msg`)
//! are dropped. `message` lines with `role=developer` / `system` are
//! also dropped — those are the harness's system prompt, not agent
//! activity.
//!
//! Unlike Claude Code, Codex lines have no per-line `uuid`. We
//! synthesize one from `payload.call_id` (function calls/results) or
//! from the file-line index (messages, reasoning), suffixed with a
//! type prefix so collisions across event kinds are impossible.

use std::time::SystemTime;

use super::{EventKind, TranscriptEvent};

pub(super) fn parse_line(line: &str, line_idx: usize) -> Vec<TranscriptEvent> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![];
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return vec![];
    }
    let timestamp = v
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_timestamp)
        .unwrap_or_else(SystemTime::now);
    let Some(payload) = v.get("payload") else {
        return vec![];
    };
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("message") => parse_message(payload, line_idx, timestamp)
            .into_iter()
            .collect(),
        Some("function_call") => parse_function_call(payload, timestamp)
            .into_iter()
            .collect(),
        Some("function_call_output") => parse_function_call_output(payload, timestamp)
            .into_iter()
            .collect(),
        Some("reasoning") => parse_reasoning(payload, line_idx, timestamp)
            .into_iter()
            .collect(),
        _ => vec![],
    }
}

fn parse_message(
    payload: &serde_json::Value,
    line_idx: usize,
    timestamp: SystemTime,
) -> Option<TranscriptEvent> {
    let role = payload.get("role").and_then(|v| v.as_str())?;
    // developer / system are harness-injected prompts; not agent
    // activity. Skip.
    if role != "user" && role != "assistant" {
        return None;
    }
    let content = payload.get("content").and_then(|v| v.as_array())?;
    let text = concat_text_blocks(content);
    if text.is_empty() {
        return None;
    }
    let uuid = format!("msg:{line_idx}");
    let kind = if role == "user" {
        EventKind::UserPrompt { content: text }
    } else {
        EventKind::AssistantText {
            text,
            model: None,
            usage: None,
            stop_reason: None,
        }
    };
    Some(TranscriptEvent {
        uuid,
        parent_uuid: None,
        timestamp,
        kind,
    })
}

fn parse_function_call(
    payload: &serde_json::Value,
    timestamp: SystemTime,
) -> Option<TranscriptEvent> {
    let call_id = payload.get("call_id").and_then(|v| v.as_str())?.to_string();
    let tool_name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Codex stores arguments as a JSON-encoded string for OpenAI
    // API parity. Parse it back to a Value so the span's
    // `gen_ai.tool.arguments` attribute is structured, not double-
    // encoded. Fall back to the raw string on parse error.
    let input = payload
        .get("arguments")
        .and_then(|v| v.as_str())
        .map(|s| serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|_| s.into()))
        .unwrap_or(serde_json::Value::Null);
    Some(TranscriptEvent {
        uuid: format!("fc:{call_id}"),
        parent_uuid: None,
        timestamp,
        kind: EventKind::ToolUse {
            tool_use_id: call_id,
            tool_name,
            input,
        },
    })
}

fn parse_function_call_output(
    payload: &serde_json::Value,
    timestamp: SystemTime,
) -> Option<TranscriptEvent> {
    let call_id = payload.get("call_id").and_then(|v| v.as_str())?.to_string();
    let content = payload
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Codex has no explicit error flag on call outputs (the agent
    // reads exit codes from the output text). Surface false so the
    // attribute is present and the consumer can override if their
    // eval rule wants to grep for "exited with code N≠0".
    Some(TranscriptEvent {
        uuid: format!("fco:{call_id}"),
        parent_uuid: Some(format!("fc:{call_id}")),
        timestamp,
        kind: EventKind::ToolResult {
            tool_use_id: call_id,
            content,
            is_error: false,
        },
    })
}

fn parse_reasoning(
    payload: &serde_json::Value,
    line_idx: usize,
    timestamp: SystemTime,
) -> Option<TranscriptEvent> {
    // Reasoning blocks carry their text in `summary[*].text` or
    // `content[*].text` depending on the rollout vintage; accept
    // both. Concatenate so a multi-segment reasoning trace becomes
    // one span (one decision = one event).
    let text = payload
        .get("summary")
        .and_then(|v| v.as_array())
        .map(|arr| concat_text_blocks(arr))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            payload
                .get("content")
                .and_then(|v| v.as_array())
                .map(|arr| concat_text_blocks(arr))
        })?;
    if text.is_empty() {
        return None;
    }
    Some(TranscriptEvent {
        uuid: format!("r:{line_idx}"),
        parent_uuid: None,
        timestamp,
        kind: EventKind::AssistantThinking { text },
    })
}

/// Pull `.text` from each block in a Codex content/summary array and
/// join with `\n`. Codex uses several content-block shapes
/// (`input_text`, `output_text`, `summary_text`); each carries the
/// payload at `.text` so a uniform projection works.
fn concat_text_blocks(arr: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for b in arr {
        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

fn parse_timestamp(s: &str) -> Option<SystemTime> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_message_into_user_prompt() {
        let line = r#"{"timestamp":"2026-05-18T09:26:21Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello codex"}]}}"#;
        let events = parse_line(line, 7);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uuid, "msg:7");
        match &events[0].kind {
            EventKind::UserPrompt { content } => assert_eq!(content, "hello codex"),
            other => panic!("expected UserPrompt, got {other:?}"),
        }
    }

    #[test]
    fn parses_assistant_message_with_output_text() {
        let line = r#"{"timestamp":"2026-05-18T09:26:30Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"running the tests"}]}}"#;
        let events = parse_line(line, 9);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            EventKind::AssistantText { text, .. } => assert_eq!(text, "running the tests"),
            other => panic!("expected AssistantText, got {other:?}"),
        }
    }

    #[test]
    fn drops_developer_and_system_messages() {
        for role in ["developer", "system"] {
            let line = format!(
                r#"{{"timestamp":"2026-05-18T09:26:21Z","type":"response_item","payload":{{"type":"message","role":"{role}","content":[{{"type":"input_text","text":"x"}}]}}}}"#,
            );
            assert!(
                parse_line(&line, 0).is_empty(),
                "expected drop for role={role}",
            );
        }
    }

    #[test]
    fn parses_function_call_into_tool_use_with_decoded_arguments() {
        let line = r#"{"timestamp":"2026-05-18T09:26:26Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"pwd\",\"workdir\":\"/tmp\"}","call_id":"call_abc"}}"#;
        let events = parse_line(line, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uuid, "fc:call_abc");
        match &events[0].kind {
            EventKind::ToolUse {
                tool_use_id,
                tool_name,
                input,
            } => {
                assert_eq!(tool_use_id, "call_abc");
                assert_eq!(tool_name, "exec_command");
                assert_eq!(input.get("cmd").and_then(|v| v.as_str()), Some("pwd"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parses_function_call_output_into_tool_result_chained_to_call() {
        let line = r#"{"timestamp":"2026-05-18T09:26:26Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_abc","output":"/tmp\n"}}"#;
        let events = parse_line(line, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uuid, "fco:call_abc");
        // function_call_output chains back to its function_call via
        // parent_uuid so a future "exact-chain" visualization can
        // pair invocation + result.
        assert_eq!(events[0].parent_uuid.as_deref(), Some("fc:call_abc"));
        match &events[0].kind {
            EventKind::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call_abc");
                assert_eq!(content, "/tmp\n");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn parses_reasoning_summary_into_assistant_thinking() {
        let line = r#"{"timestamp":"2026-05-18T09:26:25Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"first thought"},{"type":"summary_text","text":"second thought"}]}}"#;
        let events = parse_line(line, 3);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uuid, "r:3");
        match &events[0].kind {
            EventKind::AssistantThinking { text } => {
                assert_eq!(text, "first thought\nsecond thought");
            }
            other => panic!("expected AssistantThinking, got {other:?}"),
        }
    }

    #[test]
    fn drops_envelope_types() {
        for ty in ["session_meta", "turn_context", "event_msg", "unknown"] {
            let line = format!(r#"{{"timestamp":"2026-05-18T09:26:21Z","type":"{ty}"}}"#);
            assert!(parse_line(&line, 0).is_empty(), "expected drop for {ty}");
        }
    }

    #[test]
    fn malformed_input_returns_empty() {
        assert!(parse_line("", 0).is_empty());
        assert!(parse_line("not json", 0).is_empty());
        assert!(parse_line("{}", 0).is_empty());
        assert!(parse_line(r#"{"type":"response_item"}"#, 0).is_empty()); // no payload
    }
}
