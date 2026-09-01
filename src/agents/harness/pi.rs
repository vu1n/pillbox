//! pi harness adapter — `pi -p --mode json` (a `HarnessAdapter`, stdout
//! JSON-lines). See the `PiAdapter` doc for schema-verification status.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::contract::{
    Custom, EffectiveRuntimeLimitsEvidence, EvidenceUnavailableReason, MessageDelta, MessageEnd,
    MessageStart, Payload, RequestedRunProfile, Role, RunFinished, RunStarted, ServedRunProfile,
    ServedRunProfileEvidence, ToolCall, ToolStatus,
};

use super::{str_field, HarnessAdapter};

/// PiAdapter state.
#[derive(Debug, Default)]
struct PiState {
    tool_names: HashMap<String, String>,
    /// Message ids for which a `MessageStart` has already been emitted — pi
    /// streams assistant text as deltas with no terminal "id", so the
    /// normalizer synthesizes an id and emits `MessageStart` once per message.
    open_messages: HashSet<String>,
    /// Whether any assistant message terminated with an error stop-reason; pi
    /// reports failure on the message, not the terminal `agent_end`, so this
    /// carries forward to set a non-zero `RunFinished.exit_code`.
    saw_error: bool,
    /// Last concrete assistant response profile reported by pi itself. This is
    /// runtime evidence; it is never filled from the requested profile.
    served_model: Option<ServedRunProfile>,
}

/// The `pi` coding harness (https://pi.dev/, npm `@earendil-works/pi-coding-agent`)
/// via `pi -p --mode json … "<prompt>"`. pi's JSON mode emits one
/// `AgentSessionEvent` per stdout line — a clean line-delimited stream that
/// drops straight into the shared `AgentDriver`, so we pick it over the
/// stdin/stdout `--mode rpc` request/response protocol.
///
/// Schema-verification status (read before trusting the normalizer):
///   - VERIFIED against pi 0.80.10 `-p --mode json`: the session
///     header line, `agent_start`/`turn_start`, `message_start`/`message_end`,
///     `turn_end`, and `agent_end` envelope, plus the assistant `message`
///     object shape (no `id` field; carries `timestamp`, `stopReason`,
///     `errorMessage`). Captured here on a quota-blocked account, so the
///     observed assistant turns terminated with `stopReason:"error"`.
///   - VERIFIED from pi 0.80.10's shipped TypeScript declarations
///     (`@earendil-works/pi-ai` + `pi-agent-core` `.d.ts`, which JSON mode
///     serializes verbatim via `JSON.stringify(event)`): the streaming
///     `message_update.assistantMessageEvent` text lifecycle
///     (`text_start`/`text_delta`/`text_end`) and the
///     `tool_execution_start`/`tool_execution_end` event fields
///     (`toolCallId`, `toolName`, `args`, `result`, `isError`).
///   - NOT YET seen on a live wire: a *successful* assistant text turn and a
///     real tool round-trip (the test account is out of quota). The text/tool
///     branches are built to the declared `.d.ts` shapes; once a funded
///     account is available, capture `pi -p --mode json "edit a file"` and
///     re-confirm `text_delta.delta`, `tool_execution_end.result` content
///     flattening, and the `agent_end` exit semantics against these fixtures.
#[derive(Default)]
pub(crate) struct PiAdapter {
    state: PiState,
    requested: Option<RequestedRunProfile>,
}

impl PiAdapter {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn with_request(requested: RequestedRunProfile) -> Self {
        Self {
            state: PiState::default(),
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
            // Pi 0.80.10 exposes catalog capabilities, but no observed per-run
            // limits. Catalog values are not effective runtime evidence.
            effective_limits: Some(EffectiveRuntimeLimitsEvidence::Unavailable {
                reason: EvidenceUnavailableReason::NotReported,
            }),
        }
    }
}

impl HarnessAdapter for PiAdapter {
    fn run_argv(&self, prompt: &str) -> Vec<String> {
        // `-p`/`--print` is non-interactive (process the prompt and exit);
        // `--mode json` switches stdout to the JSONL event stream. `-t …`
        // allowlists every built-in tool (the read-only ones — grep/find/ls —
        // are off by default) since the sandbox is the security boundary, and
        // `--no-session` keeps the run ephemeral (no session file written into
        // the mounted workspace).
        let mut argv = vec![
            "pi".into(),
            "-p".into(),
            "--mode".into(),
            "json".into(),
            "--no-session".into(),
            "-t".into(),
            "read,bash,edit,write,grep,find,ls".into(),
        ];
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
        argv.push(prompt.into());
        argv
    }

    fn parse_line(&mut self, line: &Value) -> Vec<Payload> {
        match str_field(line, "type") {
            // The first line of `--mode json` output. It anchors the run.
            "session" => vec![Payload::RunStarted(RunStarted {
                agent: "pi".into(),
                parent_run_id: String::new(),
                base_snapshot: String::new(),
                requested: self.requested.clone(),
            })],
            // Streaming assistant output. Text arrives as a series of
            // `text_delta`s on `message_update`; the assistant `message_start`
            // carries empty content, so we drive Start/Delta/End off the inner
            // `assistantMessageEvent` text lifecycle instead.
            "message_update" => pi_message_update(line, &mut self.state),
            // `message_end` for an assistant message: close any open text
            // message and record an error stop-reason for the final exit code.
            "message_end" => pi_message_end(line, &mut self.state),
            // Tool execution — start opens a `running` ToolCall (and remembers
            // id→name), end closes it as completed/error with flattened output.
            "tool_execution_start" => pi_tool_start(line, &mut self.state),
            "tool_execution_end" => pi_tool_end(line, &mut self.state),
            // Terminal event for the whole run.
            "agent_end" => {
                if self.state.served_model.is_none() {
                    self.state.served_model = line
                        .get("messages")
                        .and_then(Value::as_array)
                        .and_then(|messages| messages.iter().rev().find_map(pi_served_profile));
                }
                vec![Payload::RunFinished(self.terminal_payload(0))]
            }
            // Auto-retry around a transient provider error — surface it as a
            // Custom event so orchestrators see the stall without it being
            // mistaken for assistant text.
            "auto_retry_start" => vec![Payload::Custom(Custom {
                name: "auto_retry".into(),
                payload: Some(line.clone()),
            })],
            _ => Vec::new(),
        }
    }
}

/// Synthesize a stable per-message id. Assistant messages carry no `id`, but
/// every event for one message shares the same `message.timestamp`, so it
/// keys the streaming text lifecycle. `contentIndex` scopes the id to one text
/// block (a turn can interleave several text blocks with tool calls).
fn pi_message_id(line: &Value) -> String {
    let ts = line
        .get("message")
        .and_then(|m| m.get("timestamp"))
        .map(|v| v.to_string())
        .unwrap_or_default();
    let idx = line
        .get("assistantMessageEvent")
        .and_then(|e| e.get("contentIndex"))
        .map(|v| v.to_string())
        .unwrap_or_default();
    format!("{ts}-{idx}")
}

/// `message_update` → drive MessageStart/Delta/End off the text lifecycle in
/// `assistantMessageEvent`. `text_start` opens the message, each `text_delta`
/// streams a chunk, `text_end` closes it. We emit a `MessageStart` lazily on
/// the first text event for a message id (some providers emit only deltas).
fn pi_message_update(line: &Value, state: &mut PiState) -> Vec<Payload> {
    let event = match line.get("assistantMessageEvent") {
        Some(e) => e,
        None => return Vec::new(),
    };
    let kind = str_field(event, "type");
    if !matches!(kind, "text_start" | "text_delta" | "text_end") {
        // thinking_*, toolcall_*, start/done/error deltas — tool calls are
        // covered by the dedicated tool_execution_* events, so ignore here.
        return Vec::new();
    }
    let message_id = pi_message_id(line);
    let mut out = Vec::new();
    if state.open_messages.insert(message_id.clone()) {
        out.push(Payload::MessageStart(MessageStart {
            message_id: message_id.clone(),
            role: Role::Assistant,
        }));
    }
    match kind {
        "text_delta" => {
            let text = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.push(Payload::MessageDelta(MessageDelta {
                message_id: message_id.clone(),
                text,
            }));
        }
        "text_end" => {
            state.open_messages.remove(&message_id);
            out.push(Payload::MessageEnd(MessageEnd::new(message_id)));
        }
        // text_start only needs the (already-emitted) MessageStart.
        _ => {}
    }
    out
}

/// `message_end` for an assistant message. Close any still-open text message
/// (defensive: covers providers that stream deltas without a `text_end`), and
/// record an error stop-reason so `agent_end` yields a non-zero exit code.
fn pi_message_end(line: &Value, state: &mut PiState) -> Vec<Payload> {
    let msg = match line.get("message") {
        Some(m) if str_field(m, "role") == "assistant" => m,
        _ => return Vec::new(),
    };
    if str_field(msg, "stopReason") == "error" {
        state.saw_error = true;
    }
    if let Some(profile) = pi_served_profile(msg) {
        state.served_model = Some(profile);
    }
    // Close any open text block keyed on this message's timestamp.
    let ts = msg
        .get("timestamp")
        .map(|v| v.to_string())
        .unwrap_or_default();
    let still_open: Vec<String> = state
        .open_messages
        .iter()
        .filter(|id| id.starts_with(&format!("{ts}-")))
        .cloned()
        .collect();
    let mut out = Vec::new();
    for id in still_open {
        state.open_messages.remove(&id);
        out.push(Payload::MessageEnd(MessageEnd::new(id)));
    }
    out
}

/// Runtime-served model from an assistant message. Pi reports the configured
/// model plus an optional `responseModel` when the provider routed to a more
/// concrete model; the latter is the stronger runtime fact. Thinking/profile
/// are intentionally absent because Pi does not report their effective values.
fn pi_served_profile(message: &Value) -> Option<ServedRunProfile> {
    if str_field(message, "role") != "assistant" {
        return None;
    }
    let model = ["responseModel", "model"].into_iter().find_map(|key| {
        message
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    })?;
    let provider = message
        .get("provider")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(ServedRunProfile {
        provider,
        model: model.to_string(),
        profile: None,
        reasoning_profile: None,
    })
}

/// `tool_execution_start` → a `running` ToolCall; remember id→name so the
/// matching `tool_execution_end` can recover the name.
fn pi_tool_start(line: &Value, state: &mut PiState) -> Vec<Payload> {
    let id = str_field(line, "toolCallId").to_string();
    let name = str_field(line, "toolName").to_string();
    state.tool_names.insert(id.clone(), name.clone());
    vec![Payload::ToolCall(ToolCall {
        tool_call_id: id,
        name,
        status: ToolStatus::Running,
        input: line.get("args").cloned(),
        output: String::new(),
        title: String::new(),
    })]
}

/// `tool_execution_end` → close the matching ToolCall. `result` is an
/// `AgentToolResult` (`{content:[…], details, …}`); flatten its `content`
/// blocks to a display string. `isError` drives the status.
fn pi_tool_end(line: &Value, state: &mut PiState) -> Vec<Payload> {
    let id = str_field(line, "toolCallId").to_string();
    let name = if str_field(line, "toolName").is_empty() {
        state.tool_names.get(&id).cloned().unwrap_or_default()
    } else {
        str_field(line, "toolName").to_string()
    };
    let is_error = line
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    vec![Payload::ToolCall(ToolCall {
        tool_call_id: id,
        name,
        status: if is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Completed
        },
        input: None,
        output: pi_tool_output(line.get("result")),
        title: String::new(),
    })]
}

/// Flatten a pi `AgentToolResult.content` (array of `{type,text}` / `{type,…}`
/// blocks) to a display string. Tolerates a bare string or a non-standard
/// `result` shape so a schema drift degrades to "stringify it" rather than
/// dropping the output.
fn pi_tool_output(result: Option<&Value>) -> String {
    match result {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => match obj.get("content") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            Some(Value::String(s)) => s.clone(),
            _ => obj.to_string(),
        },
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── pi ────────────────────────────────────────────────────────────────
    //
    // Lifecycle/envelope fixtures (`session`, `message_end`, `agent_end`, the
    // assistant `message` object) match pi 0.80.10's JSON event declarations
    // `pi -p --mode json`. The inner streaming `message_update`
    // (`assistantMessageEvent.text_*`) and `tool_execution_*` fixtures are
    // built to the package's shipped `.d.ts` type declarations (pi JSON mode
    // serializes each event verbatim) — the test account was quota-blocked, so
    // a successful text/tool turn could not be captured live. See the
    // `PiAdapter` doc-comment for the validation gap.
    fn pi_run(lines: &[Value]) -> Vec<Payload> {
        let mut a = PiAdapter::default();
        lines.iter().flat_map(|l| a.parse_line(l)).collect()
    }

    #[test]
    fn pi_exact_request_maps_to_native_provider_model_and_thinking_flags() {
        let requested = RequestedRunProfile::parse(
            "openai-codex/gpt-5.6-sol",
            Some("sol".into()),
            Some(crate::contract::ReasoningEffort::High),
        )
        .unwrap();
        let argv = PiAdapter::with_request(requested).run_argv("do it");
        assert_eq!(
            argv,
            [
                "pi",
                "-p",
                "--mode",
                "json",
                "--no-session",
                "-t",
                "read,bash,edit,write,grep,find,ls",
                "--provider",
                "openai-codex",
                "--model",
                "gpt-5.6-sol",
                "--thinking",
                "high",
                "do it",
            ]
        );
        assert!(!argv.iter().any(|arg| arg == "sol"));
    }

    #[test]
    fn pi_session_header_maps_to_run_started() {
        // Verbatim first line from `pi --mode json`.
        let out = pi_run(&[json!({
            "type":"session","version":3,"id":"019e-uuid",
            "timestamp":"2026-05-26T17:09:27.959Z","cwd":"/work"
        })]);
        assert!(matches!(out.as_slice(), [Payload::RunStarted(r)] if r.agent == "pi"));
    }

    #[test]
    fn pi_streaming_text_becomes_message_start_delta_end() {
        // text_start opens, deltas stream, text_end closes — one MessageStart.
        let out = pi_run(&[
            json!({"type":"message_update","message":{"role":"assistant","timestamp":1779815367989_i64},
                "assistantMessageEvent":{"type":"text_start","contentIndex":0,"partial":{}}}),
            json!({"type":"message_update","message":{"role":"assistant","timestamp":1779815367989_i64},
                "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hel","partial":{}}}),
            json!({"type":"message_update","message":{"role":"assistant","timestamp":1779815367989_i64},
                "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"lo","partial":{}}}),
            json!({"type":"message_update","message":{"role":"assistant","timestamp":1779815367989_i64},
                "assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"Hello","partial":{}}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(s), Payload::MessageDelta(d1), Payload::MessageDelta(d2), Payload::MessageEnd(e)] =>
            {
                assert_eq!(s.role, Role::Assistant);
                assert_eq!(d1.text, "Hel");
                assert_eq!(d2.text, "lo");
                // start/delta/end all share the synthesized id
                assert_eq!(s.message_id, d1.message_id);
                assert_eq!(s.message_id, e.message_id);
            }
            other => panic!("expected start/delta/delta/end, got {other:?}"),
        }
    }

    #[test]
    fn pi_text_delta_without_text_start_still_emits_start_once() {
        // Defensive: a provider that emits only deltas (no text_start) must
        // still yield exactly one MessageStart before the deltas.
        let out = pi_run(&[
            json!({"type":"message_update","message":{"role":"assistant","timestamp":42},
                "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"a","partial":{}}}),
            json!({"type":"message_update","message":{"role":"assistant","timestamp":42},
                "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"b","partial":{}}}),
        ]);
        let starts = out
            .iter()
            .filter(|p| matches!(p, Payload::MessageStart(_)))
            .count();
        assert_eq!(starts, 1, "exactly one MessageStart, got {out:?}");
        assert_eq!(
            out.iter()
                .filter(|p| matches!(p, Payload::MessageDelta(_)))
                .count(),
            2
        );
    }

    #[test]
    fn pi_message_end_closes_an_open_text_block() {
        // deltas arrived but no text_end; message_end must flush the open block.
        let out = pi_run(&[
            json!({"type":"message_update","message":{"role":"assistant","timestamp":7},
                "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"x","partial":{}}}),
            json!({"type":"message_end","message":{"role":"assistant","timestamp":7,"stopReason":"stop","content":[]}}),
        ]);
        assert!(
            matches!(out.last(), Some(Payload::MessageEnd(e)) if e.message_id == "7-0"),
            "expected trailing MessageEnd, got {out:?}"
        );
    }

    #[test]
    fn pi_tool_start_then_end_pairs_by_id_and_carries_name() {
        let out = pi_run(&[
            json!({"type":"tool_execution_start","toolCallId":"call_1","toolName":"bash",
                "args":{"command":"echo HELLO"}}),
            json!({"type":"tool_execution_end","toolCallId":"call_1","toolName":"bash",
                "result":{"content":[{"type":"text","text":"HELLO"}],"details":{}},"isError":false}),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(running), Payload::ToolCall(done)] => {
                assert_eq!(running.tool_call_id, "call_1");
                assert_eq!(running.name, "bash");
                assert_eq!(running.status, ToolStatus::Running);
                assert_eq!(running.input.as_ref().unwrap()["command"], "echo HELLO");
                assert_eq!(done.tool_call_id, "call_1");
                assert_eq!(done.name, "bash");
                assert_eq!(done.status, ToolStatus::Completed);
                assert_eq!(done.output, "HELLO");
            }
            other => panic!("expected two ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn pi_tool_end_recovers_name_from_state_when_omitted() {
        // tool_execution_end with an empty toolName falls back to the
        // id→name remembered at tool_execution_start.
        let out = pi_run(&[
            json!({"type":"tool_execution_start","toolCallId":"c2","toolName":"edit","args":{}}),
            json!({"type":"tool_execution_end","toolCallId":"c2","toolName":"",
                "result":{"content":[{"type":"text","text":"ok"}]},"isError":false}),
        ]);
        assert!(matches!(&out[1], Payload::ToolCall(t) if t.name == "edit"));
    }

    #[test]
    fn pi_tool_error_maps_to_error_status() {
        let out = pi_run(&[
            json!({"type":"tool_execution_end","toolCallId":"c3","toolName":"bash",
            "result":{"content":[{"type":"text","text":"boom"}]},"isError":true}),
        ]);
        assert!(matches!(out.as_slice(), [Payload::ToolCall(t)] if t.status == ToolStatus::Error));
    }

    #[test]
    fn pi_agent_end_maps_to_run_finished_ok() {
        let out = pi_run(&[json!({"type":"agent_end","messages":[],"willRetry":false})]);
        assert!(matches!(out.as_slice(), [Payload::RunFinished(r)]
        if r.exit_code == 0
            && matches!(r.served_model, Some(ServedRunProfileEvidence::Unavailable {
                reason: EvidenceUnavailableReason::NotReported
            }))
            && matches!(r.effective_limits, Some(EffectiveRuntimeLimitsEvidence::Unavailable {
                reason: EvidenceUnavailableReason::NotReported
            }))));
    }

    #[test]
    fn pi_terminal_reports_concrete_response_model_without_inferred_profile() {
        let out = pi_run(&[
            json!({"type":"message_end","message":{
                "role":"assistant","provider":"openai-codex","model":"gpt-5.6-sol",
                "responseModel":"gpt-5.6-sol-2026-07-15","stopReason":"stop",
                "timestamp":1779815367989_i64,"content":[]
            }}),
            json!({"type":"agent_end","messages":[],"willRetry":false}),
        ]);
        let Some(Payload::RunFinished(finished)) = out.last() else {
            panic!("expected terminal event, got {out:?}");
        };
        assert_eq!(
            finished.served_model,
            Some(ServedRunProfileEvidence::Reported {
                profile: ServedRunProfile {
                    provider: Some("openai-codex".into()),
                    model: "gpt-5.6-sol-2026-07-15".into(),
                    profile: None,
                    reasoning_profile: None,
                }
            })
        );
    }

    #[test]
    fn pi_agent_end_messages_are_runtime_evidence_when_message_end_was_absent() {
        let out = pi_run(&[json!({"type":"agent_end","messages":[{
            "role":"assistant","provider":"openai-codex","model":"gpt-5.6-terra",
            "stopReason":"stop","content":[],"timestamp":1
        }],"willRetry":false})]);
        assert!(matches!(out.as_slice(), [Payload::RunFinished(r)]
            if matches!(&r.served_model,
                Some(ServedRunProfileEvidence::Reported { profile })
                    if profile.model == "gpt-5.6-terra"
                        && profile.provider.as_deref() == Some("openai-codex"))));
    }

    #[test]
    fn pi_assistant_error_sets_nonzero_exit() {
        // Verbatim envelope captured from a quota-blocked pi run: the failure
        // rides on the assistant message's stopReason, and agent_end must
        // surface it as a non-zero exit code.
        let out = pi_run(&[
            json!({"type":"message_end","message":{"role":"assistant","content":[],
                "model":"claude-opus-4-7","stopReason":"error","timestamp":1779815367989_i64,
                "errorMessage":"400 invalid_request_error: out of usage"}}),
            json!({"type":"agent_end","messages":[],"willRetry":false}),
        ]);
        assert!(
            matches!(out.last(), Some(Payload::RunFinished(r)) if r.exit_code == 1),
            "expected non-zero exit, got {out:?}"
        );
    }

    #[test]
    fn pi_full_turn_emits_expected_sequence() {
        // session → text → tool round-trip → agent_end, the shape the driver
        // sees for one successful turn.
        let out = pi_run(&[
            json!({"type":"session","version":3,"id":"u","timestamp":"t","cwd":"/w"}),
            json!({"type":"agent_start"}),
            json!({"type":"turn_start"}),
            json!({"type":"message_update","message":{"role":"assistant","timestamp":1},
                "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"running","partial":{}}}),
            json!({"type":"message_update","message":{"role":"assistant","timestamp":1},
                "assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"running","partial":{}}}),
            json!({"type":"tool_execution_start","toolCallId":"t1","toolName":"bash","args":{"command":"ls"}}),
            json!({"type":"tool_execution_end","toolCallId":"t1","toolName":"bash",
                "result":{"content":[{"type":"text","text":"a.txt"}]},"isError":false}),
            json!({"type":"agent_end","messages":[],"willRetry":false}),
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
                "run_finished",
            ]
        );
    }

    #[test]
    fn pi_unknown_and_ignored_lines_produce_nothing() {
        assert!(pi_run(&[
            json!({"type":"agent_start"}),
            json!({"type":"turn_start"}),
            json!({"type":"queue_update","steering":[],"followUp":[]}),
            json!({"type":"message_update","message":{"role":"assistant","timestamp":1},
                "assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hmm","partial":{}}}),
        ])
        .is_empty());
    }
}
