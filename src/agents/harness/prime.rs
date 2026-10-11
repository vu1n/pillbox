//! Prime Agent 0.9.8 native JSONL protocol, verified against the official
//! release and PrimeIntellect-ai/prime-agent v0.9.8 (7d442aafa985).

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::contract::{
    EffectiveRuntimeLimitsEvidence, EvidenceUnavailableReason, MessageDelta, MessageEnd,
    MessageStart, Payload, RequestedRunProfile, Role, RunFinished, RunStarted, ServedRunProfile,
    ServedRunProfileEvidence, Thinking, ToolCall, ToolStatus, Usage, UsageSource,
};
use crate::execution::usage::TurnUsage;

use super::{str_field, HarnessAdapter};

#[derive(Default)]
pub(crate) struct PrimeAdapter {
    requested: Option<RequestedRunProfile>,
    seen_blocks: HashSet<String>,
    open_blocks: HashSet<String>,
    accounted_messages: HashSet<String>,
    tool_names: HashMap<String, String>,
    served_model: Option<ServedRunProfile>,
    saw_error: bool,
}

impl PrimeAdapter {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn with_request(requested: RequestedRunProfile) -> Self {
        Self {
            requested: Some(requested),
            ..Self::default()
        }
    }

    pub(crate) fn terminal_payload(&self, exit_code: i32) -> RunFinished {
        RunFinished {
            result_snapshot: String::new(),
            exit_code: if self.saw_error && exit_code == 0 {
                1
            } else {
                exit_code
            },
            served_model: Some(match self.served_model.clone() {
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

    fn message_end(&mut self, message: &Value) -> Vec<Payload> {
        if str_field(message, "role") != "assistant" {
            return Vec::new();
        }
        let timestamp = message
            .get("timestamp")
            .map(Value::to_string)
            .unwrap_or_default();
        let mut out = Vec::new();
        self.saw_error |= matches!(
            str_field(message, "stopReason"),
            "error" | "aborted" | "length"
        );
        self.served_model = message
            .get("responseModel")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(|model| {
                // `model` is configured/requested; only responseModel is upstream
                // routing evidence (openai-completions.ts in the pinned release).
                ServedRunProfile {
                    provider: message
                        .get("provider")
                        .and_then(Value::as_str)
                        .filter(|provider| !provider.is_empty())
                        .map(str::to_string),
                    model: model.to_string(),
                    profile: None,
                    reasoning_profile: None,
                }
            });
        if let Some(blocks) = message.get("content").and_then(Value::as_array) {
            for (index, block) in blocks.iter().enumerate() {
                let id = format!("prime-{timestamp}-{index}");
                match str_field(block, "type") {
                    "text" => {
                        if self.seen_blocks.insert(id.clone()) {
                            out.push(Payload::MessageStart(MessageStart {
                                message_id: id.clone(),
                                role: Role::Assistant,
                            }));
                            out.push(Payload::MessageDelta(MessageDelta {
                                message_id: id.clone(),
                                text: str_field(block, "text").into(),
                            }));
                            self.open_blocks.insert(id.clone());
                        }
                        if self.open_blocks.remove(&id) {
                            out.push(Payload::MessageEnd(MessageEnd {
                                message_id: id,
                                model: self
                                    .served_model
                                    .as_ref()
                                    .map(|profile| profile.model.clone())
                                    .unwrap_or_default(),
                                stop_reason: str_field(message, "stopReason").into(),
                            }));
                        }
                    }
                    "thinking" if self.seen_blocks.insert(id) => {
                        out.push(Payload::Thinking(Thinking {
                            text: str_field(block, "thinking").into(),
                        }));
                    }
                    _ => {}
                }
            }
        }
        if self.accounted_messages.insert(timestamp.clone()) {
            if let Some(usage) = TurnUsage::from_prime_message(message) {
                out.push(Payload::Usage(Usage {
                    message_id: format!("prime-{timestamp}"),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_input_tokens: usage.cache_read_tokens,
                    cache_creation_input_tokens: usage.cache_write_tokens,
                    cost_usd: usage.cost_usd,
                    source: UsageSource::Native,
                }));
            }
        }
        out
    }

    fn message_update(&mut self, line: &Value) -> Vec<Payload> {
        let Some(event) = line.get("assistantMessageEvent") else {
            return Vec::new();
        };
        let timestamp = line["message"]["timestamp"].to_string();
        let index = event["contentIndex"].to_string();
        let id = format!("prime-{timestamp}-{index}");
        let kind = str_field(event, "type");
        if kind == "thinking_end" && self.seen_blocks.insert(id.clone()) {
            return vec![Payload::Thinking(Thinking {
                text: str_field(event, "content").into(),
            })];
        }
        if !matches!(kind, "text_start" | "text_delta" | "text_end") {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.seen_blocks.insert(id.clone()) {
            self.open_blocks.insert(id.clone());
            out.push(Payload::MessageStart(MessageStart {
                message_id: id.clone(),
                role: Role::Assistant,
            }));
        }
        if kind == "text_delta" {
            out.push(Payload::MessageDelta(MessageDelta {
                message_id: id,
                text: str_field(event, "delta").into(),
            }));
        } else if kind == "text_end" && self.open_blocks.remove(&id) {
            out.push(Payload::MessageEnd(MessageEnd::new(id)));
        }
        out
    }
}

impl HarnessAdapter for PrimeAdapter {
    fn run_argv(&self, prompt: &str) -> Vec<String> {
        let mut argv = [
            "/usr/bin/env",
            "PRIME_AGENT_INTERNAL_LEGACY_OWNED_WORKER_FRONTEND=1",
            "prime-agent",
            "-p",
            "--mode",
            "json",
            "--no-session",
            "--offline",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
        ]
        .map(str::to_string)
        .to_vec();
        if let Some(requested) = &self.requested {
            argv.extend([
                "--provider".into(),
                requested.provider.clone(),
                "--model".into(),
                requested.model.clone(),
            ]);
            if let Some(effort) = requested.reasoning_effort {
                argv.extend([
                    "--thinking".into(),
                    match effort {
                        crate::contract::ReasoningEffort::Low => "low",
                        crate::contract::ReasoningEffort::Medium => "medium",
                        crate::contract::ReasoningEffort::High => "high",
                    }
                    .into(),
                ]);
            }
        }
        argv.extend([
            "--".into(),
            format!("Complete the following user request:\n\n{prompt}"),
        ]);
        argv
    }

    fn parse_line(&mut self, line: &Value) -> Vec<Payload> {
        // Context: doc://pillbox/agent-io-pty-free-contract@0002#agent-io-pty-free-contract — native events normalize to the shared PTY-free vocabulary.
        match str_field(line, "type") {
            "session" => vec![Payload::RunStarted(RunStarted {
                agent: "prime-agent".into(),
                parent_run_id: String::new(),
                base_snapshot: String::new(),
                requested: self.requested.clone(),
            })],
            "message_update" => self.message_update(line),
            "message_end" => self.message_end(&line["message"]),
            "tool_execution_start" | "tool_execution_end" => {
                let id = str_field(line, "toolCallId").to_string();
                let name = str_field(line, "toolName").to_string();
                let start = str_field(line, "type") == "tool_execution_start";
                if start {
                    self.tool_names.insert(id.clone(), name.clone());
                }
                let name = if name.is_empty() {
                    self.tool_names.get(&id).cloned().unwrap_or_default()
                } else {
                    name
                };
                let output = if start {
                    String::new()
                } else {
                    match line["result"].get("content").and_then(Value::as_array) {
                        Some(blocks)
                            if blocks.iter().all(|block| {
                                str_field(block, "type") == "text" && block["text"].is_string()
                            }) =>
                        {
                            blocks
                                .iter()
                                .map(|block| str_field(block, "text"))
                                .collect::<Vec<_>>()
                                .join("")
                        }
                        _ => line["result"].to_string(),
                    }
                };
                vec![Payload::ToolCall(ToolCall {
                    tool_call_id: id,
                    name,
                    status: if start {
                        ToolStatus::Running
                    } else if line["isError"] == true {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Completed
                    },
                    input: start.then(|| line["args"].clone()),
                    output,
                    title: String::new(),
                })]
            }
            "agent_end" => {
                let mut out = Vec::new();
                for message in line["messages"].as_array().into_iter().flatten() {
                    out.extend(self.message_end(message));
                }
                out.push(Payload::RunFinished(self.terminal_payload(0)));
                out
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selection_lowers_to_native_prime_provider_model_and_effort() {
        let request = RequestedRunProfile::parse(
            "prime-inference/anthropic/claude-fable-5",
            None,
            Some(crate::contract::ReasoningEffort::High),
        )
        .unwrap();
        let argv = PrimeAdapter::with_request(request).run_argv("/autonomous touch unsafe");
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--provider", "prime-inference"]));
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--model", "anthropic/claude-fable-5"]));
        assert!(argv.windows(2).any(|pair| pair == ["--thinking", "high"]));
        assert_eq!(argv[argv.len() - 2], "--");
        assert!(!argv.last().unwrap().starts_with('/'));
    }

    #[test]
    fn terminal_fallback_preserves_text_usage_and_only_observed_response_model() {
        let message = json!({"role":"assistant","timestamp":1,"model":"requested-model","provider":"prime-inference",
            "responseModel":"observed-model","stopReason":"stop","content":[{"type":"text","text":"answer"}],
            "usage":{"input":10,"output":5,"cacheRead":3,"cacheWrite":1,"cost":{"total":0.01}}});
        let mut adapter = PrimeAdapter::default();
        let out = adapter.parse_line(&json!({"type":"message_end","message":message}));
        assert!(matches!(&out[1], Payload::MessageDelta(delta) if delta.text == "answer"));
        assert!(
            matches!(out.last(), Some(Payload::Usage(usage)) if usage.input_tokens == Some(10) && usage.cost_usd == Some(0.01))
        );
        let terminal = adapter.parse_line(&json!({"type":"agent_end","messages":[message]}));
        assert_eq!(terminal.len(), 1);
        assert!(
            matches!(&terminal[0], Payload::RunFinished(finished) if matches!(&finished.served_model, Some(ServedRunProfileEvidence::Reported{profile}) if profile.model == "observed-model"))
        );
        let mut adapter = PrimeAdapter::default();
        let out = adapter.parse_line(&json!({"type":"agent_end","messages":[{"role":"assistant","timestamp":2,"model":"requested","stopReason":"length","content":[]}]}));
        assert!(
            matches!(&out[0], Payload::RunFinished(finished) if finished.exit_code == 1 && matches!(finished.served_model, Some(ServedRunProfileEvidence::Unavailable{..})))
        );
    }

    #[test]
    fn stream_and_complete_message_do_not_duplicate_text_or_usage() {
        let mut adapter = PrimeAdapter::default();
        let mut out = Vec::new();
        for event in [
            json!({"type":"text_start","contentIndex":0}),
            json!({"type":"text_delta","contentIndex":0,"delta":"answer"}),
            json!({"type":"text_end","contentIndex":0,"content":"answer"}),
        ] {
            out.extend(adapter.parse_line(&json!({"type":"message_update","message":{"timestamp":7},"assistantMessageEvent":event})));
        }
        out.extend(adapter.parse_line(&json!({"type":"message_end","message":{"role":"assistant","timestamp":7,"content":[{"type":"text","text":"answer"}],"stopReason":"stop"}})));
        assert_eq!(out.len(), 3);
        assert!(matches!(&out[1], Payload::MessageDelta(delta) if delta.text == "answer"));
    }
}
