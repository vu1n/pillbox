//! Grok Build harness adapter — `grok -p --output-format streaming-json`.
//!
//! Schema verified against the Grok Build headless guide
//! (`streaming-json` events: `text`, `thought`, `tool_call`,
//! `tool_call_update`, `usage`, `end`, `error`) in xai-org/grok-build
//! `14-headless-mode.md` and <https://docs.x.ai/build/cli/headless-scripting>.
//! Interactive runs allow tools; the text/2 driver uses a separate tool-free
//! argv in `execution::grok`.
// Context: doc://pillbox/agent-io-pty-free-contract@0002#agent-io-pty-free-contract — grok headless streaming-json is normalized into the shared event vocabulary.

use serde_json::Value;

use crate::contract::{
    EffectiveRuntimeLimitsEvidence, EvidenceUnavailableReason, MessageDelta, MessageEnd,
    MessageStart, Payload, RequestedRunProfile, Role, RunFinished, ServedRunProfile,
    ServedRunProfileEvidence, ToolCall, ToolStatus,
};

use super::{str_field, HarnessAdapter};

#[derive(Debug, Default)]
struct GrokState {
    open_message: Option<String>,
    message_seq: u64,
    saw_error: bool,
    served_model: Option<ServedRunProfile>,
}

#[derive(Default)]
pub(crate) struct GrokAdapter {
    state: GrokState,
    requested: Option<RequestedRunProfile>,
}

impl GrokAdapter {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn with_request(requested: RequestedRunProfile) -> Self {
        Self {
            state: GrokState::default(),
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

impl HarnessAdapter for GrokAdapter {
    fn run_argv(&self, prompt: &str) -> Vec<String> {
        let mut argv = vec![
            "grok".into(),
            "--no-auto-update".into(),
            "--output-format".into(),
            "streaming-json".into(),
            "--always-approve".into(),
        ];
        if let Some(requested) = &self.requested {
            argv.extend(["--model".into(), requested.model.clone()]);
            if let Some(effort) = requested.reasoning_effort {
                let effort = match effort {
                    crate::contract::ReasoningEffort::Low => "low",
                    crate::contract::ReasoningEffort::Medium => "medium",
                    crate::contract::ReasoningEffort::High => "high",
                };
                argv.extend(["--effort".into(), effort.into()]);
            }
        }
        argv.extend(["-p".into(), prompt.into()]);
        argv
    }

    fn parse_line(&mut self, line: &Value) -> Vec<Payload> {
        match str_field(line, "type") {
            "text" => grok_text(line, &mut self.state),
            "tool_call" => grok_tool(line, &mut self.state, false),
            "tool_call_update" => grok_tool(line, &mut self.state, true),
            "end" => {
                remember_served(&mut self.state, line);
                let mut out = close_message(&mut self.state);
                let failed = str_field(line, "stopReason") != "end_turn";
                if failed {
                    self.state.saw_error = true;
                }
                out.push(Payload::RunFinished(self.terminal_payload(if failed {
                    1
                } else {
                    0
                })));
                out
            }
            "error" => {
                self.state.saw_error = true;
                let mut out = close_message(&mut self.state);
                out.push(Payload::RunFinished(self.terminal_payload(1)));
                out
            }
            _ => Vec::new(),
        }
    }
}

fn remember_served(state: &mut GrokState, line: &Value) {
    let Some(usage) = line.get("modelUsage").and_then(Value::as_object) else {
        return;
    };
    let keys: Vec<&str> = usage
        .keys()
        .map(String::as_str)
        .filter(|key| !key.is_empty() && *key != "unknown")
        .collect();
    if let [model] = keys.as_slice() {
        state.served_model = Some(ServedRunProfile {
            provider: None,
            model: (*model).to_owned(),
            profile: None,
            reasoning_profile: None,
        });
    }
}

fn grok_text(line: &Value, state: &mut GrokState) -> Vec<Payload> {
    let text = str_field(line, "data");
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if state.open_message.is_none() {
        state.message_seq += 1;
        let message_id = format!("grok-{}", state.message_seq);
        state.open_message = Some(message_id.clone());
        out.push(Payload::MessageStart(MessageStart {
            message_id: message_id.clone(),
            role: Role::Assistant,
        }));
    }
    let message_id = state.open_message.clone().unwrap_or_default();
    out.push(Payload::MessageDelta(MessageDelta {
        message_id,
        text: text.to_owned(),
    }));
    out
}

fn close_message(state: &mut GrokState) -> Vec<Payload> {
    match state.open_message.take() {
        Some(id) => vec![Payload::MessageEnd(MessageEnd::new(id))],
        None => Vec::new(),
    }
}

fn grok_tool(line: &Value, state: &mut GrokState, update: bool) -> Vec<Payload> {
    let mut out = if update {
        Vec::new()
    } else {
        close_message(state)
    };
    let status = if update {
        match str_field(line, "status") {
            "error" | "failed" => ToolStatus::Error,
            _ => ToolStatus::Completed,
        }
    } else {
        ToolStatus::Running
    };
    let output = line
        .get("rawOutput")
        .map(|value| value.to_string())
        .unwrap_or_default();
    out.push(Payload::ToolCall(ToolCall {
        tool_call_id: str_field(line, "toolCallId").to_owned(),
        name: str_field(line, "toolName").to_owned(),
        status,
        input: line.get("rawInput").cloned(),
        output,
        title: str_field(line, "title").to_owned(),
    }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ReasoningEffort;
    use serde_json::json;

    #[test]
    fn interactive_argv_allows_tools() {
        let argv = GrokAdapter::default().run_argv("say hello");
        assert!(argv.iter().any(|arg| arg == "--always-approve"));
        assert!(!argv
            .iter()
            .any(|arg| arg == "--disallowed-tools" || arg == "--tools"));
        assert_eq!(argv[argv.len() - 2..], ["-p", "say hello"]);
        let requested = GrokAdapter::with_request(RequestedRunProfile {
            provider: "xai".into(),
            model: "grok-4.6".into(),
            profile: None,
            reasoning_effort: Some(ReasoningEffort::Low),
        });
        let argv = requested.run_argv("hi");
        assert!(argv.windows(2).any(|pair| pair == ["--model", "grok-4.6"]));
        assert!(argv.windows(2).any(|pair| pair == ["--effort", "low"]));
    }

    #[test]
    fn streaming_json_becomes_a_message_and_a_tool() {
        let mut adapter = GrokAdapter::default();
        let started = adapter.parse_line(&json!({"type": "text", "data": "Hello"}));
        assert!(matches!(started[0], Payload::MessageStart(_)));
        assert!(matches!(&started[1], Payload::MessageDelta(delta) if delta.text == "Hello"));
        let tool = adapter.parse_line(&json!({
            "type": "tool_call",
            "toolCallId": "c1",
            "toolName": "read_file",
            "rawInput": {"path": "a.rs"}
        }));
        assert!(matches!(&tool[0], Payload::MessageEnd(_)));
        assert!(
            matches!(&tool[1], Payload::ToolCall(call) if call.name == "read_file" && call.status == ToolStatus::Running)
        );
        let done = adapter.parse_line(&json!({
            "type": "end",
            "stopReason": "end_turn",
            "modelUsage": {"grok-4.6": {}}
        }));
        assert!(matches!(&done[0], Payload::RunFinished(finished) if finished.exit_code == 0));
        assert!(matches!(
            &done[0],
            Payload::RunFinished(finished)
                if matches!(&finished.served_model, Some(ServedRunProfileEvidence::Reported { profile }) if profile.model == "grok-4.6")
        ));
    }
}
