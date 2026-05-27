//! Claude Code harness adapter — `claude -p … --output-format stream-json`
//! (a `HarnessAdapter`, stdout JSON-lines).

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{
    Custom, MessageDelta, MessageEnd, MessageStart, Payload, Role, RunFinished, RunStarted,
    ToolCall, ToolStatus,
};

use super::{str_field, HarnessAdapter};

/// ClaudeAdapter state: tool id→name, so a `tool_result` (which carries only
/// the id) can recover the tool name.
#[derive(Debug, Default)]
struct ClaudeState {
    tool_names: HashMap<String, String>,
}

/// Claude Code via `claude -p … --output-format stream-json`. Schema verified
/// empirically against Claude Code 2.1.143.
#[derive(Default)]
pub(crate) struct ClaudeAdapter {
    state: ClaudeState,
}

impl HarnessAdapter for ClaudeAdapter {
    fn run_argv(&self, prompt: &str) -> Vec<String> {
        // `--verbose` is required alongside `stream-json`; skip-permissions is
        // safe because the run is sandboxed (and requires the non-root user the
        // agent sandbox is launched as).
        vec![
            "claude".into(),
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--dangerously-skip-permissions".into(),
        ]
    }

    fn parse_line(&mut self, line: &Value) -> Vec<Payload> {
        match str_field(line, "type") {
            "system" if str_field(line, "subtype") == "init" => {
                vec![Payload::RunStarted(RunStarted {
                    agent: "claude".into(),
                    parent_run_id: String::new(),
                    base_snapshot: String::new(),
                })]
            }
            "assistant" => assistant_blocks(line, &mut self.state),
            "user" => tool_results(line, &mut self.state),
            "result" => {
                let is_error = line
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut out = vec![Payload::RunFinished(RunFinished {
                    result_snapshot: String::new(),
                    exit_code: if is_error { 1 } else { 0 },
                })];
                // Cost/usage as a Custom event — orchestrators/Slack want spend
                // visibility, especially once `-p` bills API.
                if line.get("total_cost_usd").is_some() {
                    out.push(Payload::Custom(Custom {
                        name: "usage".into(),
                        payload: Some(serde_json::json!({
                            "total_cost_usd": line.get("total_cost_usd"),
                            "num_turns": line.get("num_turns"),
                        })),
                    }));
                }
                out
            }
            "rate_limit_event" => vec![Payload::Custom(Custom {
                name: "rate_limit".into(),
                payload: line.get("rate_limit_info").cloned(),
            })],
            _ => Vec::new(),
        }
    }
}

/// Assistant message → text blocks become MessageStart/Delta/End; tool_use
/// blocks become a `running` ToolCall (and we remember the id→name).
fn assistant_blocks(line: &Value, state: &mut ClaudeState) -> Vec<Payload> {
    let msg = &line["message"];
    let message_id = msg
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut out = Vec::new();
    for b in msg
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match str_field(b, "type") {
            "text" => {
                let text = b
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                out.push(Payload::MessageStart(MessageStart {
                    message_id: message_id.clone(),
                    role: Role::Assistant,
                }));
                out.push(Payload::MessageDelta(MessageDelta {
                    message_id: message_id.clone(),
                    text,
                }));
                out.push(Payload::MessageEnd(MessageEnd {
                    message_id: message_id.clone(),
                }));
            }
            "tool_use" => {
                let id = b
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = b
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                state.tool_names.insert(id.clone(), name.clone());
                out.push(Payload::ToolCall(ToolCall {
                    tool_call_id: id,
                    name,
                    status: ToolStatus::Running,
                    input: b.get("input").cloned(),
                    output: String::new(),
                    title: String::new(),
                }));
            }
            _ => {}
        }
    }
    out
}

/// User message → tool_result blocks close the matching ToolCall.
fn tool_results(line: &Value, state: &mut ClaudeState) -> Vec<Payload> {
    let mut out = Vec::new();
    for b in line["message"]
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if str_field(b, "type") != "tool_result" {
            continue;
        }
        let tool_call_id = b
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        out.push(Payload::ToolCall(ToolCall {
            name: state
                .tool_names
                .get(&tool_call_id)
                .cloned()
                .unwrap_or_default(),
            tool_call_id,
            status: if is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Completed
            },
            input: None,
            output: tool_result_text(b.get("content")),
            title: String::new(),
        }));
    }
    out
}

/// tool_result `content` is a string for simple tools, or an array of blocks
/// for richer ones. Flatten to a display string.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Fixtures are the exact shapes captured from Claude Code 2.1.143
    // (`claude -p … --output-format stream-json`).
    fn run(lines: &[Value]) -> Vec<Payload> {
        let mut a = ClaudeAdapter::default();
        lines.iter().flat_map(|l| a.parse_line(l)).collect()
    }

    #[test]
    fn init_maps_to_run_started() {
        let out = run(&[json!({"type":"system","subtype":"init","apiKeySource":"none"})]);
        assert!(matches!(out.as_slice(), [Payload::RunStarted(r)] if r.agent == "claude"));
    }

    #[test]
    fn tool_use_then_result_pairs_by_id_and_carries_name() {
        let out = run(&[
            json!({"type":"assistant","message":{"id":"m1","content":[
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"echo HELLO"}}]}}),
            json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"HELLO"}]}}),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(running), Payload::ToolCall(done)] => {
                assert_eq!(running.tool_call_id, "toolu_1");
                assert_eq!(running.name, "Bash");
                assert_eq!(running.status, ToolStatus::Running);
                assert_eq!(running.input.as_ref().unwrap()["command"], "echo HELLO");
                // result line carries only the id — name is recovered from state
                assert_eq!(done.tool_call_id, "toolu_1");
                assert_eq!(done.name, "Bash");
                assert_eq!(done.status, ToolStatus::Completed);
                assert_eq!(done.output, "HELLO");
            }
            other => panic!("expected two ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_error_maps_to_error_status() {
        let out = run(&[json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"x","is_error":true,"content":"boom"}]}})]);
        assert!(matches!(out.as_slice(), [Payload::ToolCall(t)] if t.status == ToolStatus::Error));
    }

    #[test]
    fn assistant_text_becomes_message_start_delta_end() {
        let out = run(&[json!({"type":"assistant","message":{"id":"m2","content":[
            {"type":"text","text":"done"}]}})]);
        match out.as_slice() {
            [Payload::MessageStart(s), Payload::MessageDelta(d), Payload::MessageEnd(e)] => {
                assert_eq!(s.role, Role::Assistant);
                assert_eq!(s.message_id, "m2");
                assert_eq!(d.text, "done");
                assert_eq!(e.message_id, "m2");
            }
            other => panic!("expected message start/delta/end, got {other:?}"),
        }
    }

    #[test]
    fn result_maps_to_run_finished_plus_usage() {
        let out = run(&[
            json!({"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.029,"num_turns":2}),
        ]);
        match out.as_slice() {
            [Payload::RunFinished(r), Payload::Custom(c)] => {
                assert_eq!(r.exit_code, 0);
                assert_eq!(c.name, "usage");
                assert_eq!(c.payload.as_ref().unwrap()["num_turns"], 2);
            }
            other => panic!("expected RunFinished + usage Custom, got {other:?}"),
        }
    }

    #[test]
    fn result_error_sets_nonzero_exit() {
        let out = run(&[json!({"type":"result","subtype":"error","is_error":true})]);
        assert!(matches!(&out[0], Payload::RunFinished(r) if r.exit_code == 1));
    }

    #[test]
    fn rate_limit_event_becomes_custom() {
        let out = run(&[json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}})]);
        assert!(matches!(out.as_slice(), [Payload::Custom(c)] if c.name == "rate_limit"));
    }

    #[test]
    fn unknown_lines_are_ignored() {
        assert!(run(&[json!({"type":"something_new","x":1})]).is_empty());
    }
}
