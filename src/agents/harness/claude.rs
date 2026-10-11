//! Claude Code harness adapter — `claude -p … --output-format stream-json`
//! (a `HarnessAdapter`, stdout JSON-lines).
//!
//! Two drivers share this normalizer:
//!   - libkrun `claude-stream` ([`crate::agents::CLAUDE_STREAM`]): a supervised
//!     one-shot in the microVM. argv comes from [`ClaudeAdapter::guest_root_argv`]
//!     (the guest runs as root, so `--permission-mode auto`, never
//!     `--dangerously-skip-permissions`); `sandbox::structured` drains the
//!     capture into the durable SessionLog.
//!   - the deprecated docker `sandbox agent` channel, argv from
//!     [`HarnessAdapter::run_argv`] (left as-is; docker gets no new work).
//!
//! Schema: first verified against Claude Code 2.1.143; re-verified against the
//! runner-pinned 2.1.289 from real stream-json captures of the 2.1.289 binary
//! talking to a scripted loopback Anthropic-API mock (no credentials). The
//! fixtures in the tests below are trimmed copies of those captures.

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{
    Custom, EffectiveRuntimeLimitsEvidence, EvidenceUnavailableReason, MessageDelta, MessageEnd,
    MessageStart, Payload, ReasoningEffort, RequestedRunProfile, Role, RunFinished, RunStarted,
    ServedRunProfile, ServedRunProfileEvidence, ToolCall, ToolStatus,
};
use crate::execution::usage::TurnUsage;

use super::{str_field, HarnessAdapter};

/// ClaudeAdapter state: tool id→name, so a `tool_result` (which carries only
/// the id) can recover the tool name; per-message text-block counts, so each
/// text block of one API message gets its own contract message id (2.1.289
/// emits one `assistant` line per content block, all sharing `message.id`).
#[derive(Debug, Default)]
struct ClaudeState {
    tool_names: HashMap<String, String>,
    text_blocks: HashMap<String, usize>,
    served_model: Option<ServedRunProfile>,
    saw_error: bool,
}

/// Claude Code via `claude -p … --output-format stream-json`.
#[derive(Default)]
pub(crate) struct ClaudeAdapter {
    state: ClaudeState,
    requested: Option<RequestedRunProfile>,
}

impl ClaudeAdapter {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn with_request(requested: Option<RequestedRunProfile>) -> Self {
        Self {
            state: ClaudeState::default(),
            requested,
        }
    }

    /// Headless argv for a guest that runs as **root** (the libkrun runner).
    /// Claude 2.1.289 refuses `--dangerously-skip-permissions` and
    /// `--permission-mode bypassPermissions` as root, so the run takes the
    /// interactive PTY `claude`'s posture (`permission_args` = its
    /// `sandbox_args`, `--permission-mode auto`) plus `--permission-prompts
    /// none`: nobody can answer a prompt in a one-shot, so anything auto mode
    /// would escalate is denied (reported as a `system/permission_denied` line)
    /// instead of stalling. `--` ends option parsing so a prompt starting with
    /// `-` is never read as a flag. Not `--bare`: bare mode never reads OAuth
    /// credentials, and the vault hands the guest a stubbed OAuth file.
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn guest_root_argv(&self, permission_args: &[&str], prompt: &str) -> Vec<String> {
        let mut argv: Vec<String> = [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
        ]
        .into_iter()
        .chain(permission_args.iter().copied())
        .chain(["--permission-prompts", "none"])
        .map(str::to_string)
        .collect();
        if let Some(requested) = &self.requested {
            argv.extend(["--model".into(), requested.model.clone()]);
            if let Some(effort) = requested.reasoning_effort {
                let level = match effort {
                    ReasoningEffort::Low => "low",
                    ReasoningEffort::Medium => "medium",
                    ReasoningEffort::High => "high",
                };
                argv.extend(["--effort".into(), level.into()]);
            }
        }
        argv.extend(["--".into(), prompt.into()]);
        argv
    }

    /// The terminal event — from the harness's `result` line, or synthesized
    /// when that line never arrived. A non-zero exit or an `is_error` result
    /// fails the run.
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
            // `modelUsage.*.contextWindow`/`maxOutputTokens` are catalog values
            // for the model, not observed per-run limits.
            effective_limits: Some(EffectiveRuntimeLimitsEvidence::Unavailable {
                reason: EvidenceUnavailableReason::NotReported,
            }),
        }
    }
}

impl HarnessAdapter for ClaudeAdapter {
    fn run_argv(&self, prompt: &str) -> Vec<String> {
        // Docker `sandbox agent` only (deprecated path, unchanged). `--verbose`
        // is required alongside `stream-json`; skip-permissions needs the
        // non-root user the docker agent sandbox is launched as. The root
        // libkrun guest uses [`ClaudeAdapter::guest_root_argv`] instead.
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
        match (str_field(line, "type"), str_field(line, "subtype")) {
            ("system", "init") => {
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
                    agent: "claude".into(),
                    parent_run_id: String::new(),
                    base_snapshot: String::new(),
                    requested: self.requested.clone(),
                })]
            }
            // Retry/denial notices: orchestrators need to see a stalled or
            // policy-blocked run without mistaking it for assistant text.
            ("system", "api_retry") => vec![custom("api_retry", line)],
            ("system", "permission_denied") => vec![custom("permission_denied", line)],
            ("assistant", _) => assistant_blocks(line, &mut self.state),
            ("user", _) => tool_results(line, &mut self.state),
            ("result", _) => {
                // `subtype` can say "success" while `is_error` is true (an API
                // error turn, `terminal_reason: "api_error"`) — `is_error` rules.
                let is_error = line
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.state.saw_error |= is_error;
                let mut out = vec![Payload::RunFinished(
                    self.terminal_payload(i32::from(is_error)),
                )];
                // Cost/usage as a Custom event — orchestrators/Slack want spend
                // visibility, especially once `-p` bills API. `turn_usage` is the
                // validated shape a text completion reports.
                let turn_usage = TurnUsage::from_claude_result(line);
                if line.get("total_cost_usd").is_some() || turn_usage.is_some() {
                    out.push(Payload::Custom(Custom {
                        name: "usage".into(),
                        payload: Some(serde_json::json!({
                            "total_cost_usd": line.get("total_cost_usd"),
                            "num_turns": line.get("num_turns"),
                            "terminal_reason": line.get("terminal_reason"),
                            "turn_usage": turn_usage,
                        })),
                    }));
                }
                if let Some(denials) = line
                    .get("permission_denials")
                    .filter(|d| d.as_array().is_some_and(|a| !a.is_empty()))
                {
                    out.push(Payload::Custom(Custom {
                        name: "permission_denials".into(),
                        payload: Some(denials.clone()),
                    }));
                }
                out
            }
            ("rate_limit_event", _) => vec![Payload::Custom(Custom {
                name: "rate_limit".into(),
                payload: line.get("rate_limit_info").cloned(),
            })],
            // Ignored: `stream_event` (only with --include-partial-messages,
            // which the libkrun one-shot does not pass — its capture is drained
            // after the run, so token deltas would only duplicate the complete
            // `assistant` lines), `system/status`, `system/informational`, hook
            // events, and anything newer.
            _ => Vec::new(),
        }
    }
}

fn custom(name: &str, line: &Value) -> Payload {
    Payload::Custom(Custom {
        name: name.into(),
        payload: Some(line.clone()),
    })
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
                // The first text block keeps the API message id; later text
                // blocks of the same message get `#n`, so every MessageStart
                // opens a distinct contract message.
                let seen = state.text_blocks.entry(message_id.clone()).or_default();
                let block_id = if *seen == 0 {
                    message_id.clone()
                } else {
                    format!("{message_id}#{seen}")
                };
                *seen += 1;
                let text = b
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                out.push(Payload::MessageStart(MessageStart {
                    message_id: block_id.clone(),
                    role: Role::Assistant,
                }));
                out.push(Payload::MessageDelta(MessageDelta {
                    message_id: block_id.clone(),
                    text,
                }));
                out.push(Payload::MessageEnd(MessageEnd::new(block_id)));
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
    // (`claude -p … --output-format stream-json`); the `*_2_1_289` tests use
    // trimmed lines captured from the 2.1.289 binary (see the module doc).
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
            json!({"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.029,"num_turns":2,
                "usage":{"input_tokens":9,"output_tokens":120,"cache_read_input_tokens":4000,"cache_creation_input_tokens":700}}),
        ]);
        match out.as_slice() {
            [Payload::RunFinished(r), Payload::Custom(c)] => {
                assert_eq!(r.exit_code, 0);
                assert_eq!(c.name, "usage");
                let payload = c.payload.as_ref().unwrap();
                assert_eq!(payload["num_turns"], 2);
                assert_eq!(
                    payload["turn_usage"],
                    json!({"cost_usd":0.029,"input_tokens":9,"output_tokens":120,
                        "cache_read_tokens":4000,"cache_write_tokens":700})
                );
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

    // ── 2.1.289 captures ──

    /// A full tool turn: init → text block → tool_use block (separate lines,
    /// same `message.id`) → tool_result → final text → result.
    #[test]
    fn tool_turn_2_1_289() {
        let mut adapter = ClaudeAdapter::default();
        let lines = [
            json!({"type":"system","subtype":"init","cwd":"/workspace/app","session_id":"a813","model":"claude-mock-1","permissionMode":"auto","apiKeySource":"ANTHROPIC_API_KEY","claude_code_version":"2.1.289","capabilities":["interrupt_receipt_v1"]}),
            json!({"type":"system","subtype":"status","status":"requesting","session_id":"a813","uuid":"u0"}),
            json!({"type":"assistant","message":{"id":"msg_mock_1","type":"message","role":"assistant","model":"claude-mock-1","content":[{"type":"text","text":"Running it."}],"stop_reason":null},"parent_tool_use_id":null,"session_id":"a813","uuid":"u1"}),
            json!({"type":"assistant","message":{"id":"msg_mock_1","type":"message","role":"assistant","model":"claude-mock-1","content":[{"type":"tool_use","id":"toolu_mock_1","name":"Bash","input":{"command":"echo HELLO","description":"say hello"}}],"stop_reason":null},"parent_tool_use_id":null,"session_id":"a813","uuid":"u2"}),
            json!({"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_mock_1","type":"tool_result","content":"HELLO","is_error":false}]},"parent_tool_use_id":null,"session_id":"a813","uuid":"u3","tool_use_result":{"stdout":"HELLO","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false}}),
            json!({"type":"assistant","message":{"id":"msg_mock_2","type":"message","role":"assistant","model":"claude-mock-1","content":[{"type":"text","text":"done"}],"stop_reason":null},"parent_tool_use_id":null,"session_id":"a813","uuid":"u4"}),
            json!({"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"done","stop_reason":"end_turn","total_cost_usd":0.00056,"permission_denials":[],"terminal_reason":"completed","api_error_status":null,"session_id":"a813"}),
        ];
        let out: Vec<Payload> = lines.iter().flat_map(|l| adapter.parse_line(l)).collect();
        let kinds: Vec<&str> = out
            .iter()
            .map(|p| match p {
                Payload::RunStarted(_) => "run_started",
                Payload::MessageStart(_) => "message_start",
                Payload::MessageDelta(_) => "message_delta",
                Payload::MessageEnd(_) => "message_end",
                Payload::ToolCall(_) => "tool_call",
                Payload::RunFinished(_) => "run_finished",
                Payload::Custom(_) => "custom",
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
                "tool_call",
                "tool_call",
                "message_start",
                "message_delta",
                "message_end",
                "run_finished",
                "custom",
            ]
        );
        match out.iter().find(|p| matches!(p, Payload::RunFinished(_))) {
            Some(Payload::RunFinished(r)) => {
                assert_eq!(r.exit_code, 0);
                assert!(matches!(
                    &r.served_model,
                    Some(ServedRunProfileEvidence::Reported { profile }) if profile.model == "claude-mock-1"
                ));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    /// 2.1.289 splits one API message into one `assistant` line per block, so
    /// two text blocks of one message must open two distinct messages.
    #[test]
    fn repeated_text_blocks_of_one_message_get_distinct_ids() {
        let out = run(&[
            json!({"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"a"}]}}),
            json!({"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"t","name":"Read","input":{}}]}}),
            json!({"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"b"}]}}),
        ]);
        let starts: Vec<&str> = out
            .iter()
            .filter_map(|p| match p {
                Payload::MessageStart(s) => Some(s.message_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(starts, ["m", "m#1"]);
    }

    /// Auto mode + `--permission-prompts none`: the denial arrives as a
    /// `system/permission_denied` line, an error tool_result, and a
    /// `permission_denials` list on the result.
    #[test]
    fn permission_denial_2_1_289() {
        let out = run(&[
            json!({"type":"assistant","message":{"id":"msg_mock_1","content":[{"type":"tool_use","id":"toolu_mock_1","name":"Bash","input":{"command":"rm -rf x","description":"run it"}}]}}),
            json!({"type":"system","subtype":"permission_denied","tool_name":"Bash","tool_use_id":"toolu_mock_1","decision_reason_type":"classifier","decision_reason":"Auto mode could not evaluate this action and is blocking it for safety","message":"Auto mode could not evaluate this action…","session_id":"6ca2"}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"Auto mode could not evaluate this action…","is_error":true,"tool_use_id":"toolu_mock_1"}]}}),
            json!({"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.0,"num_turns":2,"permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_mock_1","tool_input":{"command":"rm -rf x","description":"run it"}}],"terminal_reason":"completed"}),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(running), Payload::Custom(denied), Payload::ToolCall(failed), Payload::RunFinished(r), Payload::Custom(usage), Payload::Custom(denials)] =>
            {
                assert_eq!(running.status, ToolStatus::Running);
                assert_eq!(denied.name, "permission_denied");
                assert_eq!(
                    denied.payload.as_ref().unwrap()["tool_use_id"],
                    "toolu_mock_1"
                );
                assert_eq!(failed.status, ToolStatus::Error);
                assert_eq!(failed.name, "Bash");
                assert_eq!(r.exit_code, 0);
                assert_eq!(usage.name, "usage");
                assert_eq!(denials.name, "permission_denials");
                assert_eq!(denials.payload.as_ref().unwrap()[0]["tool_name"], "Bash");
            }
            other => panic!("unexpected denial mapping: {other:?}"),
        }
    }

    /// An API failure: `system/api_retry`, a synthetic error assistant message,
    /// and a result whose `subtype` is "success" but `is_error` is true.
    #[test]
    fn api_error_2_1_289_fails_the_run() {
        let mut adapter = ClaudeAdapter::default();
        let lines = [
            json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":1,"retry_delay_ms":595,"error_status":null,"error":"unknown","session_id":"34b5","uuid":"r1"}),
            json!({"type":"assistant","message":{"id":"81c8","model":"<synthetic>","role":"assistant","type":"message","content":[{"type":"text","text":"API Error: Connection refused"}]},"error":"server_error","is_api_error_message":true}),
            json!({"type":"result","subtype":"success","is_error":true,"num_turns":1,"total_cost_usd":0,"permission_denials":[],"terminal_reason":"api_error","api_error_status":null,"result":"API Error: Connection refused"}),
        ];
        let out: Vec<Payload> = lines.iter().flat_map(|l| adapter.parse_line(l)).collect();
        assert!(matches!(&out[0], Payload::Custom(c) if c.name == "api_retry"));
        assert!(out
            .iter()
            .any(|p| matches!(p, Payload::RunFinished(r) if r.exit_code == 1)));
        // A later synthesized terminal (e.g. the drain's fallback) stays failed.
        assert_eq!(adapter.terminal_payload(0).exit_code, 1);
    }

    /// Lines the one-shot ignores: partial-message stream events, status and
    /// informational system lines.
    #[test]
    fn stream_events_and_status_lines_are_ignored_2_1_289() {
        assert!(run(&[
            json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Runni"}},"session_id":"77e8"}),
            json!({"type":"system","subtype":"status","status":"requesting","session_id":"77e8"}),
            json!({"type":"system","subtype":"informational","content":"…","level":"warning","session_id":"77e8"}),
        ])
        .is_empty());
    }

    #[test]
    fn guest_root_argv_never_skips_permissions_and_ends_options_before_prompt() {
        let adapter = ClaudeAdapter::with_request(Some(RequestedRunProfile {
            provider: "claude".into(),
            model: "sonnet".into(),
            profile: None,
            reasoning_effort: Some(ReasoningEffort::High),
        }));
        let argv = adapter.guest_root_argv(&["--permission-mode", "auto"], "--help me");
        assert_eq!(
            argv,
            [
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "auto",
                "--permission-prompts",
                "none",
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--",
                "--help me",
            ]
        );
        assert!(!argv.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert!(!ClaudeAdapter::default()
            .guest_root_argv(&[], "x")
            .iter()
            .any(|a| a == "--model"));
    }
}
