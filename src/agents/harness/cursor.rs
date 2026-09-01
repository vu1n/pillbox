//! Cursor Agent harness adapter — `agent -p --force --trust --output-format
//! stream-json` (a `HarnessAdapter`, stdout JSON-lines). Schema verified against
//! Cursor CLI stream-json docs (cursor.com/docs/cli/reference/output-format).

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{
    EffectiveRuntimeLimitsEvidence, EvidenceUnavailableReason, MessageDelta, MessageEnd,
    MessageStart, Payload, RequestedRunProfile, Role, RunFinished, RunStarted, ServedRunProfile,
    ServedRunProfileEvidence, ToolCall, ToolStatus,
};

use super::{str_field, HarnessAdapter};

#[derive(Debug, Default)]
struct CursorState {
    tool_names: HashMap<String, String>,
    /// Open assistant message id (partial deltas share one MessageStart).
    open_message: Option<String>,
    message_seq: u64,
    saw_error: bool,
    served_model: Option<ServedRunProfile>,
}

#[derive(Default)]
pub(crate) struct CursorAdapter {
    state: CursorState,
    requested: Option<RequestedRunProfile>,
}

impl CursorAdapter {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn with_request(requested: RequestedRunProfile) -> Self {
        Self {
            state: CursorState::default(),
            requested: Some(requested),
        }
    }

    pub(crate) fn terminal_payload(&self, exit_code: i32) -> RunFinished {
        RunFinished {
            result_snapshot: String::new(),
            exit_code: if exit_code != 0 || self.state.saw_error {
                exit_code.max(1)
            } else {
                0
            },
            served_model: Some(match self.state.served_model.clone() {
                Some(profile) => ServedRunProfileEvidence::Reported { profile },
                None => ServedRunProfileEvidence::Unavailable {
                    reason: EvidenceUnavailableReason::NotReported,
                },
            }),
            effective_limits: Some(EffectiveRuntimeLimitsEvidence::Unavailable {
                reason: EvidenceUnavailableReason::NotReported,
            }),
        }
    }
}

impl HarnessAdapter for CursorAdapter {
    fn run_argv(&self, prompt: &str) -> Vec<String> {
        // `--stream-partial-output` emits character-level assistant deltas; without
        // it each assistant line is a full segment (still usable, but coarser §0).
        let mut argv = vec![
            "agent".into(),
            "-p".into(),
            "--force".into(),
            "--trust".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--stream-partial-output".into(),
        ];
        if let Some(requested) = &self.requested {
            argv.extend(["--model".into(), requested.model.clone()]);
        }
        argv.push(prompt.into());
        argv
    }

    fn parse_line(&mut self, line: &Value) -> Vec<Payload> {
        match str_field(line, "type") {
            "system" if str_field(line, "subtype") == "init" => {
                if let Some(model) = line
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    self.state.served_model = Some(ServedRunProfile {
                        provider: None,
                        model: model.to_string(),
                        profile: None,
                        reasoning_profile: None,
                    });
                }
                vec![Payload::RunStarted(RunStarted {
                    agent: "cursor".into(),
                    parent_run_id: String::new(),
                    base_snapshot: String::new(),
                    requested: self.requested.clone(),
                })]
            }
            "assistant" => cursor_assistant(line, &mut self.state),
            "tool_call" => match str_field(line, "subtype") {
                "started" => {
                    let mut out = cursor_close_open_message(&mut self.state);
                    out.extend(cursor_tool_started(line, &mut self.state));
                    out
                }
                "completed" => cursor_tool_completed(line, &mut self.state),
                _ => Vec::new(),
            },
            "result" => {
                let is_error = line
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    self.state.saw_error = true;
                }
                let mut out = cursor_close_open_message(&mut self.state);
                out.push(Payload::RunFinished(self.terminal_payload(if is_error {
                    1
                } else {
                    0
                })));
                out
            }
            _ => Vec::new(),
        }
    }
}

/// Cursor docs: with `--stream-partial-output`, keep only assistant events that
/// carry `timestamp_ms` and lack `model_call_id`. Timestamp + `model_call_id` is
/// a buffered pre-tool flush; no timestamp is the final flush — both duplicate
/// earlier deltas (drop unconditionally, even after a tool closed the message).
fn cursor_message_id(line: &Value, state: &mut CursorState) -> String {
    // Continue the open message so partial deltas share one id. Mint a new id
    // only when opening — session_id alone would collide across assistant
    // segments separated by tool calls.
    if let Some(open) = &state.open_message {
        return open.clone();
    }
    state.message_seq += 1;
    if let Some(sid) = line
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        format!("cursor-{sid}-{}", state.message_seq)
    } else {
        state.message_seq.to_string()
    }
}

fn cursor_assistant(line: &Value, state: &mut CursorState) -> Vec<Payload> {
    let has_ts = line.get("timestamp_ms").is_some();
    if !has_ts {
        return Vec::new();
    }
    if line.get("model_call_id").is_some() {
        return Vec::new();
    }
    let message_id = cursor_message_id(line, state);
    let text = line
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if state.open_message.as_deref() != Some(message_id.as_str()) {
        if let Some(prev) = state.open_message.take() {
            out.push(Payload::MessageEnd(MessageEnd::new(prev)));
        }
        state.open_message = Some(message_id.clone());
        out.push(Payload::MessageStart(MessageStart {
            message_id: message_id.clone(),
            role: Role::Assistant,
        }));
    }
    out.push(Payload::MessageDelta(MessageDelta { message_id, text }));
    out
}

fn cursor_close_open_message(state: &mut CursorState) -> Vec<Payload> {
    match state.open_message.take() {
        Some(id) => vec![Payload::MessageEnd(MessageEnd::new(id))],
        None => Vec::new(),
    }
}

fn cursor_tool_name(tool_call: &Value) -> String {
    let obj = match tool_call.as_object() {
        Some(o) => o,
        None => return "tool".into(),
    };
    if let Some(name) = obj
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
    {
        return name.to_string();
    }
    for key in obj.keys() {
        if key.ends_with("ToolCall") {
            return key
                .strip_suffix("ToolCall")
                .unwrap_or(key.as_str())
                .to_string();
        }
    }
    "tool".into()
}

fn cursor_tool_inner(tool_call: &Value) -> Option<&Value> {
    let obj = tool_call.as_object()?;
    if let Some(func) = obj.get("function") {
        return Some(func);
    }
    obj.values().next()
}

fn cursor_tool_started(line: &Value, state: &mut CursorState) -> Vec<Payload> {
    let id = str_field(line, "call_id").to_string();
    let tool_call = line.get("tool_call").unwrap_or(&Value::Null);
    let name = cursor_tool_name(tool_call);
    state.tool_names.insert(id.clone(), name.clone());
    let inner = cursor_tool_inner(tool_call);
    let input = inner.and_then(|t| t.get("args")).cloned();
    vec![Payload::ToolCall(ToolCall {
        tool_call_id: id,
        name,
        status: ToolStatus::Running,
        input,
        output: String::new(),
        title: String::new(),
    })]
}

fn cursor_tool_is_error(result: &Value) -> bool {
    result.get("error").is_some()
        || result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn cursor_tool_output(result: &Value) -> String {
    if let Some(success) = result.get("success") {
        if let Some(content) = success.get("content").and_then(Value::as_str) {
            return content.to_string();
        }
        return success.to_string();
    }
    if let Some(err) = result.get("error") {
        return err.to_string();
    }
    result.to_string()
}

fn cursor_tool_completed(line: &Value, state: &mut CursorState) -> Vec<Payload> {
    let id = str_field(line, "call_id").to_string();
    let tool_call = line.get("tool_call").unwrap_or(&Value::Null);
    let name = cursor_tool_name(tool_call);
    if name != "tool" {
        state.tool_names.insert(id.clone(), name.clone());
    }
    let resolved_name = state.tool_names.get(&id).cloned().unwrap_or(name);
    let inner = cursor_tool_inner(tool_call);
    let result = inner.and_then(|t| t.get("result"));
    let is_error = result.map(cursor_tool_is_error).unwrap_or(false);
    let output = result.map(cursor_tool_output).unwrap_or_default();
    vec![Payload::ToolCall(ToolCall {
        tool_call_id: id,
        name: resolved_name,
        status: if is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Completed
        },
        input: None,
        output,
        title: String::new(),
    })]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cursor_run(lines: &[Value]) -> Vec<Payload> {
        let mut a = CursorAdapter::default();
        lines.iter().flat_map(|l| a.parse_line(l)).collect()
    }

    #[test]
    fn cursor_run_argv_includes_headless_flags_and_model() {
        let requested = RequestedRunProfile::parse("ignored/grok-4.5", None, None).unwrap();
        let argv = CursorAdapter::with_request(requested).run_argv("fix tests");
        assert_eq!(
            argv,
            [
                "agent",
                "-p",
                "--force",
                "--trust",
                "--output-format",
                "stream-json",
                "--stream-partial-output",
                "--model",
                "grok-4.5",
                "fix tests",
            ]
        );
    }

    #[test]
    fn cursor_init_maps_to_run_started_and_records_model() {
        let out = cursor_run(&[json!({
            "type":"system","subtype":"init","apiKeySource":"login",
            "cwd":"/workspace","session_id":"sess-1","model":"composer-2.5"
        })]);
        assert!(matches!(out.as_slice(), [Payload::RunStarted(r)] if r.agent == "cursor"));
        let mut a = CursorAdapter::default();
        a.parse_line(&json!({
            "type":"system","subtype":"init","model":"composer-2.5"
        }));
        let finished = a.terminal_payload(0);
        assert!(matches!(
            finished.served_model,
            Some(ServedRunProfileEvidence::Reported { profile })
                if profile.model == "composer-2.5"
        ));
    }

    #[test]
    fn cursor_assistant_delta_opens_message_without_end() {
        let out = cursor_run(&[json!({
            "type":"assistant",
            "message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]},
            "session_id":"sess-1","timestamp_ms":42
        })]);
        match out.as_slice() {
            [Payload::MessageStart(s), Payload::MessageDelta(d)] => {
                assert_eq!(s.role, Role::Assistant);
                assert_eq!(s.message_id, "cursor-sess-1-1");
                assert_eq!(d.text, "Hello");
            }
            other => panic!("expected start/delta (end deferred), got {other:?}"),
        }
    }

    #[test]
    fn cursor_skips_duplicate_assistant_flushes() {
        let out = cursor_run(&[
            json!({
                "type":"assistant","timestamp_ms":1,
                "message":{"role":"assistant","content":[{"type":"text","text":"Hi"}]},
                "session_id":"s"
            }),
            // buffered flush before tool — duplicate
            json!({
                "type":"assistant","timestamp_ms":2,"model_call_id":"m1",
                "message":{"role":"assistant","content":[{"type":"text","text":"Hi"}]},
                "session_id":"s"
            }),
            // final flush — duplicate
            json!({
                "type":"assistant",
                "message":{"role":"assistant","content":[{"type":"text","text":"Hi"}]},
                "session_id":"s"
            }),
            json!({"type":"result","subtype":"success","is_error":false}),
        ]);
        let deltas: Vec<_> = out
            .iter()
            .filter_map(|p| match p {
                Payload::MessageDelta(d) => Some(d.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["Hi"]);
    }

    #[test]
    fn cursor_tool_started_then_completed_pairs_by_id() {
        let out = cursor_run(&[
            json!({
                "type":"tool_call","subtype":"started","call_id":"c1",
                "tool_call":{"readToolCall":{"args":{"path":"file.txt"}}}
            }),
            json!({
                "type":"tool_call","subtype":"completed","call_id":"c1",
                "tool_call":{"readToolCall":{
                    "args":{"path":"file.txt"},
                    "result":{"success":{"content":"HELLO","totalLines":1}}
                }}
            }),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(running), Payload::ToolCall(done)] => {
                assert_eq!(running.tool_call_id, "c1");
                assert_eq!(running.name, "read");
                assert_eq!(running.status, ToolStatus::Running);
                assert_eq!(done.tool_call_id, "c1");
                assert_eq!(done.name, "read");
                assert_eq!(done.status, ToolStatus::Completed);
                assert_eq!(done.output, "HELLO");
            }
            other => panic!("expected two ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn cursor_write_tool_call_maps_name_and_error() {
        let out = cursor_run(&[json!({
            "type":"tool_call","subtype":"completed","call_id":"w1",
            "tool_call":{"writeToolCall":{
                "result":{"error":{"message":"denied"}}
            }}
        })]);
        assert!(matches!(
            out.as_slice(),
            [Payload::ToolCall(t)] if t.name == "write" && t.status == ToolStatus::Error
        ));
    }

    #[test]
    fn cursor_function_tool_call_uses_function_name() {
        let out = cursor_run(&[json!({
            "type":"tool_call","subtype":"started","call_id":"f1",
            "tool_call":{"function":{"name":"grep","arguments":"{\"pattern\":\"x\"}"}}
        })]);
        assert!(matches!(
            out.as_slice(),
            [Payload::ToolCall(t)] if t.name == "grep"
        ));
    }

    #[test]
    fn cursor_result_success_maps_to_run_finished() {
        let out = cursor_run(&[json!({
            "type":"result","subtype":"success","is_error":false,"result":"done"
        })]);
        assert!(matches!(
            out.as_slice(),
            [Payload::RunFinished(r)] if r.exit_code == 0
        ));
    }

    #[test]
    fn cursor_result_error_sets_nonzero_exit() {
        let out = cursor_run(&[json!({
            "type":"result","subtype":"error","is_error":true
        })]);
        assert!(matches!(
            out.as_slice(),
            [Payload::RunFinished(r)] if r.exit_code == 1
        ));
    }

    #[test]
    fn cursor_drops_final_flush_even_after_tool_closed_message() {
        let out = cursor_run(&[
            json!({
                "type":"assistant","timestamp_ms":1,
                "message":{"role":"assistant","content":[{"type":"text","text":"Let me check."}]},
                "session_id":"s"
            }),
            json!({
                "type":"tool_call","subtype":"started","call_id":"t1",
                "tool_call":{"bashToolCall":{"args":{"command":"ls"}}}
            }),
            // Final flush for the pre-tool segment — no timestamp_ms.
            json!({
                "type":"assistant",
                "message":{"role":"assistant","content":[{"type":"text","text":"Let me check."}]},
                "session_id":"s"
            }),
            json!({"type":"result","subtype":"success","is_error":false}),
        ]);
        let deltas: Vec<_> = out
            .iter()
            .filter_map(|p| match p {
                Payload::MessageDelta(d) => Some(d.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["Let me check."]);
    }

    #[test]
    fn cursor_full_sequence_matches_docs_example() {
        let out = cursor_run(&[
            json!({"type":"system","subtype":"init","model":"sonnet-4","session_id":"s"}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hi"}]},"timestamp_ms":1,"session_id":"s"}),
            json!({"type":"tool_call","subtype":"started","call_id":"t1","tool_call":{"bashToolCall":{"args":{"command":"ls"}}}}),
            json!({"type":"tool_call","subtype":"completed","call_id":"t1","tool_call":{"bashToolCall":{"result":{"success":{"content":"a.txt"}}}}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done"}]},"timestamp_ms":2,"session_id":"s"}),
            json!({"type":"result","subtype":"success","is_error":false}),
        ]);
        let kinds: Vec<&str> = out
            .iter()
            .map(|p| match p {
                Payload::RunStarted(_) => "run_started",
                Payload::MessageStart(_) => "message_start",
                Payload::MessageDelta(_) => "message_delta",
                Payload::MessageEnd(_) => "message_end",
                Payload::ToolCall(t) if t.status == ToolStatus::Running => "tool_running",
                Payload::ToolCall(_) => "tool_done",
                Payload::RunFinished(_) => "run_finished",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "run_started",
                "message_start",
                "message_delta",
                "message_end",
                "tool_running",
                "tool_done",
                "message_start",
                "message_delta",
                "message_end",
                "run_finished",
            ]
        );
    }
}
