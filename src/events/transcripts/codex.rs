//! Codex (OpenAI CLI) transcript JSONL parser.
//!
//! Codex writes one JSONL file per session at
//! `~/.codex/sessions/<year>/<month>/<day>/rollout-<id>.jsonl`. Each
//! line has a `type`-tagged envelope; the meaty content sits under
//! `type: "response_item"` where `payload.type` discriminates:
//!
//! - `message` (role=user) → user prompt. Codex uses OpenAI's
//!   `input_text` content blocks; we concatenate their `.text` fields.
//! - `function_call` / `custom_tool_call` → tool invocation (name,
//!   arguments or input, call_id).
//! - `function_call_output` / `custom_tool_call_output` → tool result
//!   (call_id, string or structured output).
//! - `reasoning` → assistant thinking trace.
//! - `event_msg.task_complete` → the final assistant message plus the
//!   explicit turn-complete boundary. This is the only trustworthy idle
//!   marker in a Codex rollout: a tool-using turn can contain several
//!   intermediate assistant `response_item.message` records.
//! - `event_msg.token_count` → cumulative token accounting. The parser
//!   emits only the component-wise delta from the last accounted snapshot,
//!   so live readers and cost reducers see each token exactly once.
//!
//! Other envelope-only types (`session_meta`, `turn_context`) and `event_msg`
//! variants other than `task_complete` / `token_count` are dropped. `message` lines with
//! `role=assistant` / `developer` / `system` are also dropped: assistant output
//! is emitted once from `task_complete`, while developer/system are harness
//! prompts.
//!
//! Unlike Claude Code, Codex lines have no per-line `uuid`. We
//! synthesize one from `payload.call_id` (function calls/results), `turn_id`
//! (task completion), or the file-line index (messages, reasoning), suffixed
//! with a type prefix so collisions across event kinds are impossible.

use std::time::SystemTime;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::{EventKind, TranscriptEvent};

/// The cumulative counters Codex writes in `event_msg.token_count.info`.
/// `input_tokens` is inclusive of `cached_input_tokens`; the canonical
/// contract stores their difference as billable/non-cached input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct CumulativeUsage {
    pub(super) input_tokens: u64,
    pub(super) cached_input_tokens: u64,
    pub(super) cache_write_input_tokens: Option<u64>,
    pub(super) output_tokens: u64,
}

/// State that must travel with the detached transcript cursor. Without the
/// accounted cumulative baseline, restarting after a committed prefix would
/// either rebill the prefix or silently lose the next delta.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ParserState {
    pub(super) accounted: Option<CumulativeUsage>,
}

#[derive(Debug, Clone)]
struct TokenSnapshot {
    usage: CumulativeUsage,
}

/// Stateful Codex rollout parser. Token-count records are cumulative and may
/// be emitted repeatedly while a tool loop is active, so parsing cannot be
/// stateless once the transcript is a live durable source.
#[derive(Debug, Clone, Default)]
pub(super) struct Parser {
    state: ParserState,
}

impl Parser {
    pub(super) fn from_state(state: Option<ParserState>) -> Self {
        Self {
            state: state.unwrap_or_default(),
        }
    }

    pub(super) fn state(&self) -> ParserState {
        self.state.clone()
    }

    pub(super) fn parse_line_checked(
        &mut self,
        line: &str,
        line_idx: usize,
    ) -> anyhow::Result<Vec<TranscriptEvent>> {
        let v = serde_json::from_str::<serde_json::Value>(line)
            .with_context(|| format!("decode Codex transcript line {line_idx}"))?;
        if v.get("type").and_then(|t| t.as_str()) == Some("event_msg") {
            let timestamp = v
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(parse_timestamp)
                .unwrap_or_else(SystemTime::now);
            let Some(payload) = v.get("payload") else {
                return Ok(vec![]);
            };
            return match payload.get("type").and_then(|v| v.as_str()) {
                Some("task_complete") => Ok(parse_task_complete(&v, payload, line_idx)
                    .into_iter()
                    .collect::<Vec<_>>()),
                Some("token_count") => self.parse_token_count(payload, line_idx, timestamp),
                _ => Ok(vec![]),
            };
        }
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            return Ok(vec![]);
        }
        let timestamp = v
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_timestamp)
            .unwrap_or_else(SystemTime::now);
        let Some(payload) = v.get("payload") else {
            return Ok(vec![]);
        };
        Ok(match payload.get("type").and_then(|v| v.as_str()) {
            Some("message") => parse_message(payload, line_idx, timestamp)
                .into_iter()
                .collect(),
            Some("function_call") => parse_function_call(payload, timestamp)
                .into_iter()
                .collect(),
            Some("function_call_output") => parse_function_call_output(payload, timestamp)
                .into_iter()
                .collect(),
            Some("custom_tool_call") => parse_custom_tool_call(payload, timestamp)
                .into_iter()
                .collect(),
            Some("custom_tool_call_output") => parse_custom_tool_call_output(payload, timestamp)
                .into_iter()
                .collect(),
            Some("reasoning") => parse_reasoning(payload, line_idx, timestamp)
                .into_iter()
                .collect(),
            _ => vec![],
        })
    }

    fn parse_token_count(
        &mut self,
        payload: &serde_json::Value,
        line_idx: usize,
        timestamp: SystemTime,
    ) -> anyhow::Result<Vec<TranscriptEvent>> {
        let Some(snapshot) = parse_token_snapshot(payload)? else {
            return Ok(vec![]);
        };
        let previous = self.state.accounted.as_ref();
        let had_previous = previous.is_some();
        if let Some(previous) = previous {
            if !snapshot.usage.is_monotonic_from(previous) {
                // A decreasing cumulative stream is not a fresh session: the
                // cursor pins the rollout identity. Refuse to reset or clamp
                // it, so a malformed record cannot silently corrupt spend.
                anyhow::bail!(
                    "Codex cumulative token_count decreased at transcript line {line_idx}"
                );
            }
        }
        let cache_write_reported = snapshot.usage.cache_write_input_tokens.is_some()
            && previous.is_none_or(|previous| previous.cache_write_input_tokens.is_some());
        let mut usage = snapshot.usage;
        // A missing/null cache-write counter means "not reported for this
        // snapshot", not "the cumulative counter reset". Carry the last
        // known baseline so a later reappearance cannot rebill the prefix.
        if usage.cache_write_input_tokens.is_none() {
            usage.cache_write_input_tokens =
                previous.and_then(|previous| previous.cache_write_input_tokens);
        }
        let delta = usage.delta_from(previous);
        self.state.accounted = Some(usage);
        if delta.is_zero() && had_previous {
            return Ok(vec![]);
        }
        let usage = delta.into_gen_ai_usage(cache_write_reported);
        Ok(vec![TranscriptEvent {
            uuid: format!("usage:{line_idx}"),
            parent_uuid: None,
            timestamp,
            kind: EventKind::Usage { usage },
        }])
    }
}

impl CumulativeUsage {
    fn non_cached_input(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }

    fn is_monotonic_from(&self, previous: &Self) -> bool {
        self.input_tokens >= previous.input_tokens
            && self.cached_input_tokens >= previous.cached_input_tokens
            && self.output_tokens >= previous.output_tokens
            && self.non_cached_input() >= previous.non_cached_input()
            && match (
                self.cache_write_input_tokens,
                previous.cache_write_input_tokens,
            ) {
                (Some(current), Some(previous)) => current >= previous,
                (None, _) => true,
                (Some(_), None) => true,
            }
    }

    fn delta_from(&self, previous: Option<&Self>) -> Self {
        let previous = previous.cloned().unwrap_or_default();
        Self {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            cache_write_input_tokens: match (
                self.cache_write_input_tokens,
                previous.cache_write_input_tokens,
            ) {
                (Some(current), Some(previous)) => Some(current.saturating_sub(previous)),
                (Some(current), None) => Some(current),
                (None, _) => None,
            },
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
        }
    }

    fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_write_input_tokens.is_none_or(|value| value == 0)
            && self.output_tokens == 0
    }

    fn into_gen_ai_usage(
        self,
        cache_write_reported: bool,
    ) -> crate::events::otel::genai::GenAiUsage {
        crate::events::otel::genai::GenAiUsage {
            // Codex's input counter is inclusive of cache hits. Keep the
            // canonical contract's input field billable/non-cached.
            input_tokens: Some(self.non_cached_input()),
            output_tokens: Some(self.output_tokens),
            cache_read_input_tokens: Some(self.cached_input_tokens),
            cache_creation_input_tokens: cache_write_reported
                .then_some(self.cache_write_input_tokens.unwrap_or(0)),
            ..Default::default()
        }
    }
}

fn parse_token_snapshot(payload: &serde_json::Value) -> anyhow::Result<Option<TokenSnapshot>> {
    let Some(info) = payload.get("info") else {
        return Ok(None);
    };
    // Some Codex versions send a null info placeholder while no usage is
    // available yet. It is a no-op, not a zero-cost observation.
    let Some(info) = info.as_object() else {
        if info.is_null() {
            return Ok(None);
        }
        anyhow::bail!("Codex token_count info must be an object or null");
    };
    let Some(total) = info.get("total_token_usage") else {
        return Ok(None);
    };
    let Some(total) = total.as_object() else {
        if total.is_null() {
            return Ok(None);
        }
        anyhow::bail!("Codex token_count total_token_usage must be an object or null");
    };
    let input_tokens = required_counter(total, "input_tokens")?;
    let cached_input_tokens = required_counter(total, "cached_input_tokens")?;
    let output_tokens = required_counter(total, "output_tokens")?;
    if cached_input_tokens > input_tokens {
        anyhow::bail!(
            "Codex token_count cached_input_tokens ({cached_input_tokens}) exceeds input_tokens ({input_tokens})"
        );
    }
    let cache_write_input_tokens = counter(total, "cache_write_input_tokens")?;
    Ok(Some(TokenSnapshot {
        usage: CumulativeUsage {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
        },
    }))
}

fn counter(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> anyhow::Result<Option<u64>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("Codex token_count {key} must be a non-negative integer"))
        .map(Some)
}

fn required_counter(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> anyhow::Result<u64> {
    let Some(value) = object.get(key) else {
        anyhow::bail!("Codex token_count total_token_usage missing {key}");
    };
    if value.is_null() {
        anyhow::bail!("Codex token_count {key} must be a non-negative integer");
    }
    value
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("Codex token_count {key} must be a non-negative integer"))
}

/// Compatibility helper for one-line callers and parser unit tests. Live and
/// durable paths use [`Parser`] so cumulative accounting survives a pump or
/// process restart.
#[cfg(test)]
pub(super) fn parse_line(line: &str, line_idx: usize) -> Vec<TranscriptEvent> {
    Parser::default()
        .parse_line_checked(line, line_idx)
        .unwrap_or_default()
}

fn parse_message(
    payload: &serde_json::Value,
    line_idx: usize,
    timestamp: SystemTime,
) -> Option<TranscriptEvent> {
    let role = payload.get("role").and_then(|v| v.as_str())?;
    // Assistant response items can be intermediate text in a tool-using turn.
    // Emit the final answer exactly once from `event_msg.task_complete` below,
    // which also carries the explicit idle boundary. Developer/system are
    // harness prompts, not activity.
    if role != "user" {
        return None;
    }
    let content = payload.get("content").and_then(|v| v.as_array())?;
    let text = concat_text_blocks(content);
    if text.is_empty() {
        return None;
    }
    let uuid = format!("msg:{line_idx}");
    Some(TranscriptEvent {
        uuid,
        parent_uuid: None,
        timestamp,
        kind: EventKind::UserPrompt { content: text },
    })
}

fn parse_task_complete(
    v: &serde_json::Value,
    payload: &serde_json::Value,
    line_idx: usize,
) -> Option<TranscriptEvent> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("task_complete") {
        return None;
    }
    let text = payload
        .get("last_agent_message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Keep an empty final-message event: task_complete is also the idle boundary.
    let turn_id = payload
        .get("turn_id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| line_idx.to_string());
    let timestamp = v
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(parse_timestamp)
        .unwrap_or_else(SystemTime::now);
    Some(TranscriptEvent {
        uuid: format!("turn:{turn_id}"),
        parent_uuid: None,
        timestamp,
        kind: EventKind::AssistantText {
            text,
            model: None,
            usage: None,
            stop_reason: Some("end_turn".into()),
        },
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

fn parse_custom_tool_call(
    payload: &serde_json::Value,
    timestamp: SystemTime,
) -> Option<TranscriptEvent> {
    let call_id = payload.get("call_id").and_then(|v| v.as_str())?.to_string();
    let tool_name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let input = payload
        .get("input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Some(TranscriptEvent {
        uuid: format!("ctc:{call_id}"),
        parent_uuid: None,
        timestamp,
        kind: EventKind::ToolUse {
            tool_use_id: call_id,
            tool_name,
            input,
        },
    })
}

fn parse_custom_tool_call_output(
    payload: &serde_json::Value,
    timestamp: SystemTime,
) -> Option<TranscriptEvent> {
    let call_id = payload.get("call_id").and_then(|v| v.as_str())?.to_string();
    let output = payload
        .get("output")
        .map(stringify_tool_output)
        .unwrap_or_default();
    let is_error = payload
        .get("is_error")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            payload
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s == "error" || s == "failed")
        })
        .unwrap_or(false);
    Some(TranscriptEvent {
        uuid: format!("ctco:{call_id}"),
        parent_uuid: Some(format!("ctc:{call_id}")),
        timestamp,
        kind: EventKind::ToolResult {
            tool_use_id: call_id,
            content: output,
            is_error,
        },
    })
}

fn stringify_tool_output(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    if let Some(blocks) = value.as_array() {
        let pure_text = blocks.iter().all(|block| {
            let Some(object) = block.as_object() else {
                return false;
            };
            object.len() == 2
                && object
                    .get("type")
                    .and_then(|value| value.as_str())
                    .is_some_and(|kind| matches!(kind, "input_text" | "output_text" | "text"))
                && object.get("text").is_some_and(|value| value.is_string())
        });
        if pure_text {
            return concat_text_blocks(blocks);
        }
    }
    serde_json::to_string(value).unwrap_or_default()
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
    fn defers_assistant_messages_until_task_complete() {
        let line = r#"{"timestamp":"2026-05-18T09:26:30Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"running the tests"}]}}"#;
        assert!(parse_line(line, 9).is_empty());
    }

    #[test]
    fn task_complete_emits_final_answer_and_idle_boundary() {
        use crate::contract::{AttentionReason, Payload};

        let line = r#"{"timestamp":"2026-05-18T09:26:31Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-7","last_agent_message":"running the tests"}}"#;
        let events = parse_line(line, 9);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uuid, "turn:turn-7");
        match &events[0].kind {
            EventKind::AssistantText {
                text, stop_reason, ..
            } => {
                assert_eq!(text, "running the tests");
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
            }
            other => panic!("expected AssistantText, got {other:?}"),
        }
        let payloads = super::super::contract_map::to_payloads(&events[0]);
        assert!(matches!(
            payloads.last(),
            Some(Payload::AttentionRequired(a))
                if a.reason == AttentionReason::NeedsInput
        ));
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
    fn parses_modern_custom_tool_call_and_structured_output() {
        let call = r#"{"timestamp":"2026-09-08T06:39:44Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"const r = await tools.exec_command({cmd:\"pwd\"});","call_id":"call_modern"}}"#;
        let events = parse_line(call, 12);
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            EventKind::ToolUse {
                tool_use_id,
                tool_name,
                input,
            } => {
                assert_eq!(tool_use_id, "call_modern");
                assert_eq!(tool_name, "exec");
                assert!(input
                    .as_str()
                    .is_some_and(|text| text.starts_with("const r")));
            }
            other => panic!("expected custom ToolUse, got {other:?}"),
        }

        let output = r#"{"timestamp":"2026-09-08T06:39:44Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_modern","output":[{"type":"input_text","text":"Script completed\n"},{"type":"input_text","text":"/workspace/safe\n"}]}}"#;
        let events = parse_line(output, 14);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].parent_uuid.as_deref(), Some("ctc:call_modern"));
        match &events[0].kind {
            EventKind::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "call_modern");
                assert_eq!(content, "Script completed\n\n/workspace/safe\n");
                assert!(!is_error);
            }
            other => panic!("expected custom ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn preserves_mixed_custom_tool_output_as_json() {
        let line = r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_mixed","output":[{"type":"input_text","text":"summary"},{"type":"image","data":"opaque"}]}}"#;
        let events = parse_line(line, 1);
        let EventKind::ToolResult { content, .. } = &events[0].kind else {
            panic!("expected custom result: {events:?}");
        };
        assert!(content.contains("\"type\":\"image\""));
        assert!(content.contains("opaque"));
    }

    fn token_count(
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        cache_write_input_tokens: Option<serde_json::Value>,
    ) -> String {
        let mut total = serde_json::json!({
            "input_tokens": input_tokens,
            "cached_input_tokens": cached_input_tokens,
            "output_tokens": output_tokens,
        });
        if let Some(value) = cache_write_input_tokens {
            total["cache_write_input_tokens"] = value;
        }
        serde_json::json!({
            "timestamp": "2026-09-08T06:39:44Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": total,
                    "model_context_window": 258400
                }
            }
        })
        .to_string()
    }

    fn usage_from(
        line: &str,
        parser: &mut Parser,
        line_idx: usize,
    ) -> Option<crate::events::otel::genai::GenAiUsage> {
        let events = parser
            .parse_line_checked(line, line_idx)
            .expect("valid token count");
        events.into_iter().find_map(|event| match event.kind {
            EventKind::Usage { usage } => Some(usage),
            _ => None,
        })
    }

    #[test]
    fn emits_incremental_usage_without_repeating_cumulative_snapshots() {
        let mut parser = Parser::default();
        let first = usage_from(
            &token_count(1_000, 800, 10, Some(serde_json::json!(4))),
            &mut parser,
            15,
        )
        .expect("first usage");
        assert_eq!(first.input_tokens, Some(200));
        assert_eq!(first.cache_read_input_tokens, Some(800));
        assert_eq!(first.cache_creation_input_tokens, Some(4));
        assert_eq!(first.output_tokens, Some(10));

        assert!(usage_from(
            &token_count(1_000, 800, 10, Some(serde_json::json!(4))),
            &mut parser,
            16
        )
        .is_none());
        let second = usage_from(
            &token_count(1_250, 1_000, 14, Some(serde_json::json!(7))),
            &mut parser,
            21,
        )
        .expect("changed usage");
        assert_eq!(second.input_tokens, Some(50));
        assert_eq!(second.cache_read_input_tokens, Some(200));
        assert_eq!(second.cache_creation_input_tokens, Some(3));
        assert_eq!(second.output_tokens, Some(4));
    }

    #[test]
    fn preserves_cache_write_baseline_when_a_snapshot_omits_it() {
        let mut parser = Parser::default();
        assert!(usage_from(
            &token_count(100, 50, 1, Some(serde_json::json!(10))),
            &mut parser,
            1,
        )
        .is_some());
        let second = usage_from(&token_count(110, 55, 2, None), &mut parser, 2)
            .expect("changed usage without cache-write reporting");
        assert_eq!(second.cache_creation_input_tokens, None);
        let third = usage_from(
            &token_count(120, 60, 3, Some(serde_json::json!(12))),
            &mut parser,
            3,
        )
        .expect("cache write reappears");
        assert_eq!(third.cache_creation_input_tokens, Some(2));
    }

    #[test]
    fn first_zero_snapshot_is_a_measured_usage_and_duplicate_is_suppressed() {
        let mut parser = Parser::default();
        let zero = usage_from(
            &token_count(0, 0, 0, Some(serde_json::json!(0))),
            &mut parser,
            1,
        )
        .expect("zero is still an observation");
        assert_eq!(zero.input_tokens, Some(0));
        assert_eq!(zero.output_tokens, Some(0));
        assert!(usage_from(
            &token_count(0, 0, 0, Some(serde_json::json!(0))),
            &mut parser,
            2
        )
        .is_none());
    }

    #[test]
    fn null_info_is_a_noop_but_malformed_and_decreasing_usage_fail_loudly() {
        let mut parser = Parser::default();
        let null_info = r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        assert!(parser.parse_line_checked(null_info, 1).unwrap().is_empty());
        let malformed = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":"bad","cached_input_tokens":0,"output_tokens":1}}}}"#;
        let before_malformed = parser.state();
        let error = parser
            .parse_line_checked(malformed, 2)
            .expect_err("malformed numeric info must not disappear");
        assert!(error.to_string().contains("input_tokens"));
        assert_eq!(parser.state(), before_malformed);

        usage_from(&token_count(100, 50, 2, None), &mut parser, 3);
        let before_decreasing = parser.state();
        let decreasing = token_count(90, 40, 1, None);
        let error = parser
            .parse_line_checked(&decreasing, 4)
            .expect_err("decreasing cumulative totals must stop the producer");
        assert!(error.to_string().contains("decreased"));
        assert_eq!(parser.state(), before_decreasing);

        let invalid_cache = token_count(100, 101, 3, None);
        let error = parser
            .parse_line_checked(&invalid_cache, 5)
            .expect_err("cached input cannot exceed inclusive input");
        assert!(error.to_string().contains("exceeds input_tokens"));
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
        for ty in ["session_meta", "turn_context", "unknown"] {
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
