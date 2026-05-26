//! Harness adapters — per-coding-harness integration behind one trait, so
//! the agent channel stays harness-agnostic.
//!
//! A *harness* is a coding agent CLI (claude, pi, opencode, …). Each speaks
//! its own structured-output dialect; this trait abstracts the two things
//! that actually vary for a headless run:
//!
//!   - [`HarnessAdapter::run_argv`] — how to launch a headless, structured run.
//!   - [`HarnessAdapter::parse_line`] — how to map one line of its structured
//!     stdout to the canonical [`crate::contract`] events (the normalizer).
//!
//! The *driver* (in `commands/sandbox.rs`) is shared across harnesses that
//! stream JSON lines over stdout (claude `-p`, pi `--mode json`): docker-exec
//! the `run_argv`, read stdout line by line, feed each to `parse_line`, emit
//! the events. opencode's HTTP-`serve` model is a future second driver variant
//! — deliberately not forced into this trait off one example.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::contract::{
    Custom, MessageDelta, MessageEnd, MessageStart, Payload, Role, RunFinished, RunStarted,
    ToolCall, ToolStatus,
};

/// Cross-line state a normalizer accumulates within one run — chiefly the
/// `tool_use_id → name` map, since a harness's tool-*result* line usually
/// carries only the id, not the tool name.
#[derive(Debug, Default)]
pub(crate) struct HarnessState {
    tool_names: HashMap<String, String>,
    /// Message ids for which a `MessageStart` has already been emitted in
    /// this run. Streaming harnesses (pi) deliver assistant text as a series
    /// of deltas with no terminal "id" field, so the normalizer synthesizes
    /// an id and uses this set to emit `MessageStart` exactly once per
    /// message before the first delta. ClaudeAdapter doesn't touch it (it
    /// builds Start/Delta/End from one complete `assistant` line).
    open_messages: HashSet<String>,
    /// Whether any assistant message in this run terminated with an error
    /// stop-reason. pi reports the failure on the assistant message rather
    /// than on the terminal `agent_end` event, so the normalizer carries the
    /// signal forward to set a non-zero `RunFinished.exit_code`.
    saw_error: bool,
}

/// One coding-harness integration.
pub(crate) trait HarnessAdapter {
    /// argv for a headless, structured-output run of `prompt`, exec'd inside
    /// the sandbox. Must run non-interactively and auto-allow tools (the
    /// sandbox is the security boundary).
    fn run_argv(&self, prompt: &str) -> Vec<String>;

    /// Map one line of the harness's structured stdout to zero or more
    /// contract events. `state` persists across lines within a run.
    fn parse_line(&self, line: &Value, state: &mut HarnessState) -> Vec<Payload>;
}

/// Resolve a harness adapter by agent id.
pub(crate) fn lookup(id: &str) -> Option<Box<dyn HarnessAdapter>> {
    match id {
        "claude" => Some(Box::new(ClaudeAdapter)),
        "pi" => Some(Box::new(PiAdapter)),
        _ => None,
    }
}

/// Claude Code via `claude -p … --output-format stream-json`. Schema verified
/// empirically against Claude Code 2.1.143.
pub(crate) struct ClaudeAdapter;

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

    fn parse_line(&self, line: &Value, state: &mut HarnessState) -> Vec<Payload> {
        match str_field(line, "type") {
            "system" if str_field(line, "subtype") == "init" => {
                vec![Payload::RunStarted(RunStarted {
                    agent: "claude".into(),
                    parent_run_id: String::new(),
                    base_snapshot: String::new(),
                })]
            }
            "assistant" => assistant_blocks(line, state),
            "user" => tool_results(line, state),
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
fn assistant_blocks(line: &Value, state: &mut HarnessState) -> Vec<Payload> {
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
fn tool_results(line: &Value, state: &mut HarnessState) -> Vec<Payload> {
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

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

// ── pi ───────────────────────────────────────────────────────────────────────

/// The `pi` coding harness (https://pi.dev/, npm `@earendil-works/pi-coding-agent`)
/// via `pi -p --mode json … "<prompt>"`. pi's JSON mode emits one
/// `AgentSessionEvent` per stdout line — a clean line-delimited stream that
/// drops straight into the shared `AgentDriver`, so we pick it over the
/// stdin/stdout `--mode rpc` request/response protocol.
///
/// Schema-verification status (read before trusting the normalizer):
///   - VERIFIED empirically against pi 0.75.5 `-p --mode json`: the session
///     header line, `agent_start`/`turn_start`, `message_start`/`message_end`,
///     `turn_end`, and `agent_end` envelope, plus the assistant `message`
///     object shape (no `id` field; carries `timestamp`, `stopReason`,
///     `errorMessage`). Captured here on a quota-blocked account, so the
///     observed assistant turns terminated with `stopReason:"error"`.
///   - VERIFIED from the package's shipped TypeScript declarations
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
pub(crate) struct PiAdapter;

impl HarnessAdapter for PiAdapter {
    fn run_argv(&self, prompt: &str) -> Vec<String> {
        // `-p`/`--print` is non-interactive (process the prompt and exit);
        // `--mode json` switches stdout to the JSONL event stream. `-t …`
        // allowlists every built-in tool (the read-only ones — grep/find/ls —
        // are off by default) since the sandbox is the security boundary, and
        // `--no-session` keeps the run ephemeral (no session file written into
        // the mounted workspace).
        vec![
            "pi".into(),
            "-p".into(),
            "--mode".into(),
            "json".into(),
            "--no-session".into(),
            "-t".into(),
            "read,bash,edit,write,grep,find,ls".into(),
            prompt.into(),
        ]
    }

    fn parse_line(&self, line: &Value, state: &mut HarnessState) -> Vec<Payload> {
        match str_field(line, "type") {
            // The first line of `--mode json` output. It anchors the run.
            "session" => vec![Payload::RunStarted(RunStarted {
                agent: "pi".into(),
                parent_run_id: String::new(),
                base_snapshot: String::new(),
            })],
            // Streaming assistant output. Text arrives as a series of
            // `text_delta`s on `message_update`; the assistant `message_start`
            // carries empty content, so we drive Start/Delta/End off the inner
            // `assistantMessageEvent` text lifecycle instead.
            "message_update" => pi_message_update(line, state),
            // `message_end` for an assistant message: close any open text
            // message and record an error stop-reason for the final exit code.
            "message_end" => pi_message_end(line, state),
            // Tool execution — start opens a `running` ToolCall (and remembers
            // id→name), end closes it as completed/error with flattened output.
            "tool_execution_start" => pi_tool_start(line, state),
            "tool_execution_end" => pi_tool_end(line, state),
            // Terminal event for the whole run.
            "agent_end" => {
                vec![Payload::RunFinished(RunFinished {
                    result_snapshot: String::new(),
                    exit_code: if state.saw_error { 1 } else { 0 },
                })]
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
fn pi_message_update(line: &Value, state: &mut HarnessState) -> Vec<Payload> {
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
            out.push(Payload::MessageEnd(MessageEnd { message_id }));
        }
        // text_start only needs the (already-emitted) MessageStart.
        _ => {}
    }
    out
}

/// `message_end` for an assistant message. Close any still-open text message
/// (defensive: covers providers that stream deltas without a `text_end`), and
/// record an error stop-reason so `agent_end` yields a non-zero exit code.
fn pi_message_end(line: &Value, state: &mut HarnessState) -> Vec<Payload> {
    let msg = match line.get("message") {
        Some(m) if str_field(m, "role") == "assistant" => m,
        _ => return Vec::new(),
    };
    if str_field(msg, "stopReason") == "error" {
        state.saw_error = true;
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
        out.push(Payload::MessageEnd(MessageEnd { message_id: id }));
    }
    out
}

/// `tool_execution_start` → a `running` ToolCall; remember id→name so the
/// matching `tool_execution_end` can recover the name.
fn pi_tool_start(line: &Value, state: &mut HarnessState) -> Vec<Payload> {
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
fn pi_tool_end(line: &Value, state: &mut HarnessState) -> Vec<Payload> {
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

    // Fixtures are the exact shapes captured from Claude Code 2.1.143
    // (`claude -p … --output-format stream-json`).
    fn run(lines: &[Value]) -> Vec<Payload> {
        let a = ClaudeAdapter;
        let mut st = HarnessState::default();
        lines
            .iter()
            .flat_map(|l| a.parse_line(l, &mut st))
            .collect()
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

    // ── pi ────────────────────────────────────────────────────────────────
    //
    // Lifecycle/envelope fixtures (`session`, `message_end`, `agent_end`, the
    // assistant `message` object) are the exact shapes captured from pi 0.75.5
    // `pi -p --mode json`. The inner streaming `message_update`
    // (`assistantMessageEvent.text_*`) and `tool_execution_*` fixtures are
    // built to the package's shipped `.d.ts` type declarations (pi JSON mode
    // serializes each event verbatim) — the test account was quota-blocked, so
    // a successful text/tool turn could not be captured live. See the
    // `PiAdapter` doc-comment for the validation gap.
    fn pi_run(lines: &[Value]) -> Vec<Payload> {
        let a = PiAdapter;
        let mut st = HarnessState::default();
        lines
            .iter()
            .flat_map(|l| a.parse_line(l, &mut st))
            .collect()
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
        assert!(matches!(out.as_slice(), [Payload::RunFinished(r)] if r.exit_code == 0));
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
