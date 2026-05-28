//! Claude Code transcript JSONL parser.
//!
//! Claude Code writes one JSONL file per session at
//! `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`. Each line
//! is one event with a stable envelope (`uuid`, `parentUuid`,
//! `timestamp`, `sessionId`, `cwd`) plus a `type`-tagged payload.
//!
//! This parser only emits events we render as spans:
//!
//! - `type: "user"` with string content → [`EventKind::UserPrompt`]
//! - `type: "user"` with content blocks → one [`EventKind::ToolResult`]
//!   per `tool_result` block
//! - `type: "assistant"` → one event per `message.content[]` block:
//!   `text` → [`EventKind::AssistantText`] (carries the trailing
//!   message-level model/usage/stop_reason), `thinking` →
//!   [`EventKind::AssistantThinking`], `tool_use` →
//!   [`EventKind::ToolUse`]
//!
//! Envelope-only types (`mode`, `permission-mode`, `attachment`,
//! `file-history-snapshot`, `system`, `ai-title`, `last-prompt`) are
//! dropped — they describe harness state, not agent activity. Add
//! them later if a consumer asks.

use std::time::SystemTime;

use super::{AssistantUsage, EventKind, TranscriptEvent};

/// Parse one JSONL line into zero-or-more [`TranscriptEvent`]s. A
/// single Claude Code line can fan out: an assistant message with
/// `text` + `tool_use` blocks becomes two events sharing the line's
/// timestamp and `parent_uuid`. Returns `vec![]` for envelope-only
/// types and for malformed input — best-effort parsing so a single
/// bad line doesn't break draining.
pub(super) fn parse_line(line: &str) -> Vec<TranscriptEvent> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![];
    };
    let Some(line_uuid) = v.get("uuid").and_then(|v| v.as_str()).map(str::to_owned) else {
        return vec![];
    };
    let parent_uuid = v
        .get("parentUuid")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let timestamp = v
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_timestamp)
        .unwrap_or_else(SystemTime::now);

    match v.get("type").and_then(|v| v.as_str()) {
        Some("user") => parse_user(&v, &line_uuid, parent_uuid, timestamp),
        Some("assistant") => parse_assistant(&v, &line_uuid, parent_uuid, timestamp),
        _ => vec![],
    }
}

fn parse_user(
    v: &serde_json::Value,
    line_uuid: &str,
    parent_uuid: Option<String>,
    timestamp: SystemTime,
) -> Vec<TranscriptEvent> {
    let Some(content) = v.pointer("/message/content") else {
        return vec![];
    };

    // Most user lines carry a plain string prompt.
    if let Some(text) = content.as_str() {
        return vec![TranscriptEvent {
            uuid: line_uuid.to_string(),
            parent_uuid,
            timestamp,
            kind: EventKind::UserPrompt {
                content: text.to_string(),
            },
        }];
    }

    // Tool results arrive as user lines whose content is an array of
    // blocks. One event per `tool_result` block.
    let Some(arr) = content.as_array() else {
        return vec![];
    };
    let mut out = Vec::new();
    for block in arr {
        if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
            continue;
        }
        let tool_use_id = block
            .get("tool_use_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let content_text = block
            .get("content")
            .map(stringify_block_content)
            .unwrap_or_default();
        let is_error = block
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push(TranscriptEvent {
            uuid: format!("{line_uuid}:{tool_use_id}"),
            parent_uuid: parent_uuid.clone(),
            timestamp,
            kind: EventKind::ToolResult {
                tool_use_id,
                content: content_text,
                is_error,
            },
        });
    }
    out
}

fn parse_assistant(
    v: &serde_json::Value,
    line_uuid: &str,
    parent_uuid: Option<String>,
    timestamp: SystemTime,
) -> Vec<TranscriptEvent> {
    let Some(blocks) = v.pointer("/message/content").and_then(|v| v.as_array()) else {
        return vec![];
    };
    let model = v
        .pointer("/message/model")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let usage = v.pointer("/message/usage").map(parse_usage);
    let stop_reason = v
        .pointer("/message/stop_reason")
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let mut out = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        // Each block becomes its own span; suffix the line uuid with
        // the block index so multiple blocks under one assistant
        // message get distinct span ids.
        let block_uuid = format!("{line_uuid}:{idx}");
        let kind = match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => EventKind::AssistantText {
                text: block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                model: model.clone(),
                usage: usage.clone(),
                stop_reason: stop_reason.clone(),
            },
            Some("thinking") => EventKind::AssistantThinking {
                text: block
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("tool_use") => EventKind::ToolUse {
                tool_use_id: block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                tool_name: block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                input: block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            },
            _ => continue,
        };
        out.push(TranscriptEvent {
            uuid: block_uuid,
            parent_uuid: parent_uuid.clone(),
            timestamp,
            kind,
        });
    }
    out
}

fn parse_usage(v: &serde_json::Value) -> AssistantUsage {
    AssistantUsage {
        input_tokens: v.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: v.get("output_tokens").and_then(|v| v.as_u64()),
        cache_read_input_tokens: v.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
        cache_creation_input_tokens: v
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64()),
    }
}

/// Render a tool_result `content` field to a single string. The
/// Claude Code format allows both a string and an array of blocks
/// (the latter when a tool returned structured / mixed content);
/// for now we render either as one string so the span's
/// `gen_ai.tool.result` attribute is uniformly typed.
fn stringify_block_content(v: &serde_json::Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    serde_json::to_string(v).unwrap_or_default()
}

/// ISO8601 → SystemTime. Tolerant of the `Z` and `+HH:MM` suffixes
/// Claude Code emits. `None` on parse error — caller falls back to
/// `now()` so a malformed line still produces a span with a
/// reasonable timestamp.
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
    fn parses_user_string_prompt_as_user_prompt() {
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-05-28T10:00:00Z","message":{"role":"user","content":"hello world"}}"#;
        let events = parse_line(line);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.uuid, "u1");
        assert!(e.parent_uuid.is_none());
        match &e.kind {
            EventKind::UserPrompt { content } => assert_eq!(content, "hello world"),
            other => panic!("expected UserPrompt, got {other:?}"),
        }
    }

    #[test]
    fn parses_user_content_array_as_tool_results() {
        let line = r#"{"type":"user","uuid":"u2","parentUuid":"u1","timestamp":"2026-05-28T10:00:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_abc","content":"output text","is_error":false}]}}"#;
        let events = parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            EventKind::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "tu_abc");
                assert_eq!(content, "output text");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // Synthesized uuid includes the tool_use_id so two tool_results
        // on the same line produce distinct spans.
        assert_eq!(events[0].uuid, "u2:tu_abc");
    }

    #[test]
    fn parses_assistant_blocks_into_one_event_each() {
        let line = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-05-28T10:00:02Z","message":{"model":"claude-opus-4-7","role":"assistant","content":[{"type":"thinking","thinking":"reasoning bits"},{"type":"text","text":"here you go"},{"type":"tool_use","id":"tu_xyz","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0}}}"#;
        let events = parse_line(line);
        assert_eq!(events.len(), 3);

        match &events[0].kind {
            EventKind::AssistantThinking { text } => assert_eq!(text, "reasoning bits"),
            other => panic!("expected AssistantThinking, got {other:?}"),
        }
        match &events[1].kind {
            EventKind::AssistantText {
                text,
                model,
                usage,
                stop_reason,
            } => {
                assert_eq!(text, "here you go");
                assert_eq!(model.as_deref(), Some("claude-opus-4-7"));
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
                let u = usage.as_ref().expect("usage");
                assert_eq!(u.input_tokens, Some(100));
                assert_eq!(u.output_tokens, Some(50));
                assert_eq!(u.cache_read_input_tokens, Some(1000));
            }
            other => panic!("expected AssistantText, got {other:?}"),
        }
        match &events[2].kind {
            EventKind::ToolUse {
                tool_use_id,
                tool_name,
                input,
            } => {
                assert_eq!(tool_use_id, "tu_xyz");
                assert_eq!(tool_name, "Bash");
                assert_eq!(input.get("command").and_then(|v| v.as_str()), Some("ls"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }

        // Block index suffix keeps span ids distinct under the same
        // line uuid.
        assert_eq!(events[0].uuid, "a1:0");
        assert_eq!(events[1].uuid, "a1:1");
        assert_eq!(events[2].uuid, "a1:2");
    }

    #[test]
    fn drops_envelope_only_types() {
        for ty in [
            "mode",
            "permission-mode",
            "attachment",
            "file-history-snapshot",
            "system",
            "ai-title",
            "last-prompt",
            "unknown-future-type",
        ] {
            let line =
                format!(r#"{{"type":"{ty}","uuid":"x","timestamp":"2026-05-28T10:00:00Z"}}"#);
            assert!(parse_line(&line).is_empty(), "expected empty for type {ty}",);
        }
    }

    #[test]
    fn malformed_input_returns_empty() {
        assert!(parse_line("").is_empty());
        assert!(parse_line("not json").is_empty());
        assert!(parse_line("{}").is_empty()); // missing uuid + type
        assert!(parse_line(r#"{"type":"user"}"#).is_empty()); // no uuid
    }
}
