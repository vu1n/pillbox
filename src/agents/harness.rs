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
//! the events.
//!
//! opencode integrates differently — it has no headless stdout-JSON mode;
//! instead `opencode serve` runs an HTTP server (REST + an SSE event stream).
//! That model gets a *second* driver (`ServeDriver` in `commands/sandbox.rs`),
//! not the stdio one. But the normalizer concept is reused as-is: an opencode
//! SSE event is just another `serde_json::Value`, so [`OpencodeAdapter`]
//! implements [`ServeAdapter::parse_event`] — the same one-event →
//! zero-or-more contract [`Payload`]s shape as [`HarnessAdapter::parse_line`].
//! `ServeDriver` feeds the SSE stream through it exactly like `AgentDriver`
//! feeds stdout lines through [`ClaudeAdapter`].

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::contract::{
    AgentPhase, AttentionReason, AttentionRequired, Custom, MessageDelta, MessageEnd, MessageStart,
    Payload, PermissionRequested, PhaseChanged, Role, RunFinished, RunStarted, Todo, TodoStatus,
    TodosUpdated, ToolCall, ToolStatus,
};

// Per-run normalizer state — each adapter owns its own. The adapters are
// stateful (`parse_*` takes `&mut self`); a fresh adapter per run starts clean.

/// ClaudeAdapter state: tool id→name, so a `tool_result` (which carries only
/// the id) can recover the tool name.
#[derive(Debug, Default)]
struct ClaudeState {
    tool_names: HashMap<String, String>,
}

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
}

/// OpencodeAdapter state. opencode redelivers whole parts and overlapping
/// event families, so the normalizer dedupes against this.
#[derive(Debug, Default)]
struct OpencodeState {
    tool_names: HashMap<String, String>,
    /// Last `ToolStatus` emitted per tool call id, so a status change emits one
    /// event and an unchanged re-delivery emits none.
    tool_status: HashMap<String, ToolStatus>,
    /// Message ids we've already emitted a `MessageStart` for, so repeated
    /// `message.part.updated` deliveries for the same text part don't restart it.
    started_messages: HashSet<String>,
    /// Per-text-part last-emitted full text, so a re-delivered part only emits
    /// the *new* suffix as a `MessageDelta` (opencode redelivers the whole
    /// accumulated text each `message.part.updated`).
    part_text: HashMap<String, String>,
    /// id→role (`message.updated` arrives before a message's parts) so text
    /// parts of the *user's own prompt* aren't re-emitted as assistant output.
    message_roles: HashMap<String, Role>,
}

/// A coding-harness integration whose headless run streams structured JSON
/// **lines over stdout** (claude `-p`, pi `--mode json`). Driven by
/// `AgentDriver` in `commands/sandbox.rs`.
pub(crate) trait HarnessAdapter {
    /// argv for a headless, structured-output run of `prompt`, exec'd inside
    /// the sandbox. Must run non-interactively and auto-allow tools (the
    /// sandbox is the security boundary).
    fn run_argv(&self, prompt: &str) -> Vec<String>;

    /// Map one line of the harness's structured stdout to zero or more
    /// contract events. The adapter carries its own state across lines, so a
    /// fresh adapter is one run.
    fn parse_line(&mut self, line: &Value) -> Vec<Payload>;
}

/// A coding-harness integration that runs an **HTTP server** (`opencode
/// serve`: REST + an SSE event stream) rather than streaming JSON over
/// stdout. Driven by `ServeDriver` in `commands/sandbox.rs`.
///
/// The transport differs (HTTP/SSE, not a pipe) but the *normalizer* concept
/// is identical to [`HarnessAdapter::parse_line`]: one harness event →
/// zero-or-more contract [`Payload`]s, with the adapter carrying state across
/// events.
pub(crate) trait ServeAdapter {
    /// argv that starts the harness's HTTP server inside the sandbox, bound to
    /// `port` on loopback. The `ServeDriver` runs this detached and then
    /// connects to `127.0.0.1:<port>` from inside the container.
    fn serve_argv(&self, port: u16) -> Vec<String>;

    /// Map one harness SSE event (`{id, type, properties}`) to zero or more
    /// contract events. The adapter carries its own state across events.
    fn parse_event(&mut self, event: &Value) -> Vec<Payload>;

    /// Did this event signal the agent turn is complete? `ServeDriver` stops
    /// consuming the stream once a terminal event arrives. (opencode's
    /// `session.idle` — the turn is done and the model is no longer busy.)
    fn is_terminal(&self, event: &Value) -> bool;
}

/// Resolve a stdout-streaming harness adapter by agent id.
pub(crate) fn lookup(id: &str) -> Option<Box<dyn HarnessAdapter>> {
    match id {
        "claude" => Some(Box::new(ClaudeAdapter::default())),
        "pi" => Some(Box::new(PiAdapter::default())),
        _ => None,
    }
}

/// Resolve a serve-based harness adapter by agent id.
pub(crate) fn lookup_serve(id: &str) -> Option<Box<dyn ServeAdapter>> {
    match id {
        "opencode" => Some(Box::new(OpencodeAdapter::default())),
        _ => None,
    }
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
#[derive(Default)]
pub(crate) struct PiAdapter {
    state: PiState,
}

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

    fn parse_line(&mut self, line: &Value) -> Vec<Payload> {
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
                vec![Payload::RunFinished(RunFinished {
                    result_snapshot: String::new(),
                    exit_code: if self.state.saw_error { 1 } else { 0 },
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
fn pi_message_end(line: &Value, state: &mut PiState) -> Vec<Payload> {
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

// ── opencode (serve) ─────────────────────────────────────────────────────

/// opencode via `opencode serve` (HTTP REST + SSE event stream). The SSE
/// envelope is `{"id":"evt_…","type":"<dotted.type>","properties":{…}}`.
///
/// Schema sources (mixed, flagged below): the wire **envelope** and the
/// `message.*`/`session.*`/`permission.*`/`todo.*` events were captured
/// empirically from `opencode serve` 1.15.10 (`GET /event`); the
/// `session.next.*` fine-grained streaming family and the tool-`state` shapes
/// are taken from the server's live OpenAPI doc (`GET /doc`) of the same
/// build — opencode only emits the `session.next.*` family on some
/// model/provider paths, so those branches are schema-verified, not
/// turn-verified here.
///
/// opencode emits overlapping event families for the same content; the
/// normalizer picks one canonical source per concern to avoid double-counting:
///   - `message.part.updated` — the part-snapshot family (always emitted, the
///     canonical text/tool source). A `part` is text / reasoning / tool;
///     opencode re-delivers the *whole* accumulated part on each update, so
///     the normalizer diffs against `state` to emit only new text and one tool
///     event per status change.
///   - `message.part.delta` — incremental text deltas for the *same* parts
///     `message.part.updated` snapshots. Deliberately ignored (consuming both
///     would double the text); see the match arm.
///   - `session.next.text.*` / `session.next.tool.*` — the fine-grained
///     streaming family (emitted on some provider paths instead of, or
///     alongside, the snapshot family). These map one-to-one onto the
///     contract's message/tool events. Tool ids (`callID`) are stable, so the
///     same dedup keeps a tool from being emitted twice if both families fire.
#[derive(Default)]
pub(crate) struct OpencodeAdapter {
    state: OpencodeState,
}

impl ServeAdapter for OpencodeAdapter {
    fn serve_argv(&self, port: u16) -> Vec<String> {
        vec![
            "opencode".into(),
            "serve".into(),
            "--hostname".into(),
            "127.0.0.1".into(),
            "--port".into(),
            port.to_string(),
        ]
    }

    fn is_terminal(&self, event: &Value) -> bool {
        str_field(event, "type") == "session.idle"
    }

    fn parse_event(&mut self, event: &Value) -> Vec<Payload> {
        let state = &mut self.state;
        let props = event.get("properties").unwrap_or(event);
        match str_field(event, "type") {
            "session.created" => vec![Payload::RunStarted(RunStarted {
                agent: "opencode".into(),
                parent_run_id: String::new(),
                base_snapshot: String::new(),
            })],
            // Record id→role so text parts of the user's own prompt aren't
            // re-emitted as assistant output. Emits nothing itself.
            "message.updated" => {
                if let Some(info) = props.get("info") {
                    let id = str_field(info, "id").to_string();
                    if !id.is_empty() {
                        state
                            .message_roles
                            .insert(id, opencode_role(str_field(info, "role")));
                    }
                }
                Vec::new()
            }
            // The part-snapshot family — always emitted.
            "message.part.updated" => opencode_part(props.get("part"), state),
            // `message.part.delta` carries incremental `{partID, field, delta}`
            // for the SAME part that `message.part.updated` re-snapshots — both
            // fire for one text part (verified against opencode 1.15.4 in the
            // runner image). Normalizing both would double the text, so the
            // snapshot path above is canonical and the delta path is ignored.
            "message.part.delta" => Vec::new(),
            // session.status carries {status:{type:"busy"|"idle"}} — surface
            // as an ephemeral-ish PhaseChanged so a live UI can render
            // "thinking" vs "waiting" without inventing semantics.
            "session.status" => {
                let phase = match str_field(props.get("status").unwrap_or(props), "type") {
                    "busy" => AgentPhase::Thinking,
                    "idle" => AgentPhase::WaitingInput,
                    _ => return Vec::new(),
                };
                vec![Payload::PhaseChanged(PhaseChanged { phase })]
            }
            "session.idle" => vec![Payload::RunFinished(RunFinished {
                result_snapshot: String::new(),
                exit_code: 0,
            })],
            "session.error" | "session.next.step.failed" => {
                let msg = opencode_error_message(props.get("error"));
                vec![Payload::AttentionRequired(AttentionRequired {
                    reason: AttentionReason::ErrorStalled,
                    message: msg,
                })]
            }
            "permission.asked" => {
                let p = props;
                vec![Payload::PermissionRequested(PermissionRequested {
                    permission_id: str_field(p, "id").to_string(),
                    tool: str_field(p, "tool").to_string(),
                    description: opencode_permission_desc(p),
                    input: p.get("metadata").cloned(),
                })]
            }
            "todo.updated" => vec![Payload::TodosUpdated(TodosUpdated {
                todos: opencode_todos(props.get("todos")),
            })],
            // ── session.next.* fine-grained streaming family ──
            "session.next.text.delta" => {
                // No per-part id on these events; key the synthetic message on
                // the session so start/delta/end pair up within a turn.
                let mid = format!("msg:{}", str_field(props, "sessionID"));
                let delta = str_field(props, "delta").to_string();
                let mut out = Vec::new();
                if state.started_messages.insert(mid.clone()) {
                    out.push(Payload::MessageStart(MessageStart {
                        message_id: mid.clone(),
                        role: Role::Assistant,
                    }));
                }
                out.push(Payload::MessageDelta(MessageDelta {
                    message_id: mid,
                    text: delta,
                }));
                out
            }
            "session.next.text.ended" => {
                let mid = format!("msg:{}", str_field(props, "sessionID"));
                let mut out = Vec::new();
                // If we never saw a delta (ended-only path), synthesize the
                // whole message from the `text` field.
                if state.started_messages.insert(mid.clone()) {
                    out.push(Payload::MessageStart(MessageStart {
                        message_id: mid.clone(),
                        role: Role::Assistant,
                    }));
                    out.push(Payload::MessageDelta(MessageDelta {
                        message_id: mid.clone(),
                        text: str_field(props, "text").to_string(),
                    }));
                }
                out.push(Payload::MessageEnd(MessageEnd { message_id: mid }));
                out
            }
            "session.next.tool.input.started" => {
                let call_id = str_field(props, "callID").to_string();
                let name = str_field(props, "name").to_string();
                state.tool_names.insert(call_id.clone(), name.clone());
                tool_event(
                    state,
                    &call_id,
                    &name,
                    ToolStatus::Running,
                    None,
                    String::new(),
                )
            }
            "session.next.tool.called" => {
                let call_id = str_field(props, "callID").to_string();
                let name = str_field(props, "tool").to_string();
                state.tool_names.insert(call_id.clone(), name.clone());
                tool_event(
                    state,
                    &call_id,
                    &name,
                    ToolStatus::Running,
                    props.get("input").cloned(),
                    String::new(),
                )
            }
            "session.next.tool.success" => {
                let call_id = str_field(props, "callID").to_string();
                let name = state
                    .tool_names
                    .get(&call_id)
                    .cloned()
                    .unwrap_or_else(|| str_field(props, "tool").to_string());
                let output = opencode_tool_content(props.get("content"));
                tool_event(state, &call_id, &name, ToolStatus::Completed, None, output)
            }
            "session.next.tool.failed" => {
                let call_id = str_field(props, "callID").to_string();
                let name = state
                    .tool_names
                    .get(&call_id)
                    .cloned()
                    .unwrap_or_else(|| str_field(props, "tool").to_string());
                let output = opencode_error_message(props.get("error"));
                tool_event(state, &call_id, &name, ToolStatus::Error, None, output)
            }
            "session.next.step.ended" => opencode_usage(props),
            _ => Vec::new(),
        }
    }
}

/// Normalize one opencode message `part` (from `message.part.updated`). Text
/// and reasoning parts become message start/delta/end (diffed against state so
/// a re-delivered part only emits the new suffix); tool parts become a
/// ToolCall whose status tracks the part's `state.status`.
fn opencode_part(part: Option<&Value>, state: &mut OpencodeState) -> Vec<Payload> {
    let Some(part) = part else {
        return Vec::new();
    };
    match str_field(part, "type") {
        "text" | "reasoning" => {
            let part_id = str_field(part, "id").to_string();
            if part_id.is_empty() {
                return Vec::new();
            }
            // Only assistant text is conversation output; skip the user's own
            // prompt part (and any system part). Default to assistant when the
            // role is unknown — better to over-surface than drop a real reply.
            let role = state
                .message_roles
                .get(str_field(part, "messageID"))
                .copied()
                .unwrap_or(Role::Assistant);
            if !matches!(role, Role::Assistant) {
                return Vec::new();
            }
            let full = str_field(part, "text").to_string();
            let prev = state.part_text.get(&part_id).cloned().unwrap_or_default();
            // opencode re-sends the whole accumulated text each update; emit
            // only the new suffix. If the new text isn't an extension of the
            // prior (rare; a rewrite), fall back to sending it whole.
            let delta = full
                .strip_prefix(&prev)
                .map(str::to_string)
                .unwrap_or(full.clone());
            state.part_text.insert(part_id.clone(), full);
            if delta.is_empty() {
                return Vec::new();
            }
            let mut out = Vec::new();
            if state.started_messages.insert(part_id.clone()) {
                out.push(Payload::MessageStart(MessageStart {
                    message_id: part_id.clone(),
                    role: Role::Assistant,
                }));
            }
            out.push(Payload::MessageDelta(MessageDelta {
                message_id: part_id,
                text: delta,
            }));
            out
        }
        "tool" => {
            let call_id = str_field(part, "callID").to_string();
            let name = str_field(part, "tool").to_string();
            if !call_id.is_empty() && !name.is_empty() {
                state.tool_names.insert(call_id.clone(), name.clone());
            }
            let st = part.get("state");
            let (status, output) = match str_field(st.unwrap_or(part), "status") {
                "completed" => (
                    ToolStatus::Completed,
                    st.and_then(|s| s.get("output"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                "error" => (
                    ToolStatus::Error,
                    st.and_then(|s| s.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                // pending + running both map to Running for the contract.
                "running" | "pending" => (ToolStatus::Running, String::new()),
                _ => return Vec::new(),
            };
            let input = st.and_then(|s| s.get("input")).cloned();
            tool_event(state, &call_id, &name, status, input, output)
        }
        _ => Vec::new(), // step-start, step-finish, snapshot, patch, …
    }
}

/// Emit a ToolCall only on first sight or a status change, mirroring the
/// stdout adapter's "running → completed" pairing but for the serve transport
/// where the same tool is re-delivered as its state advances.
fn tool_event(
    state: &mut OpencodeState,
    call_id: &str,
    name: &str,
    status: ToolStatus,
    input: Option<Value>,
    output: String,
) -> Vec<Payload> {
    if call_id.is_empty() {
        return Vec::new();
    }
    if state.tool_status.get(call_id) == Some(&status) {
        return Vec::new();
    }
    state.tool_status.insert(call_id.to_string(), status);
    vec![Payload::ToolCall(ToolCall {
        tool_call_id: call_id.to_string(),
        name: name.to_string(),
        status,
        input,
        output,
        title: String::new(),
    })]
}

/// opencode message `role` strings → contract `Role`.
fn opencode_role(role: &str) -> Role {
    match role {
        "assistant" => Role::Assistant,
        "user" => Role::User,
        "system" => Role::System,
        _ => Role::Unspecified,
    }
}

/// opencode todo `status` strings → contract `TodoStatus`.
fn opencode_todos(todos: Option<&Value>) -> Vec<Todo> {
    todos
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| Todo {
            text: str_field(t, "content").to_string(),
            status: match str_field(t, "status") {
                "pending" => TodoStatus::Pending,
                "in_progress" => TodoStatus::InProgress,
                "completed" | "cancelled" => TodoStatus::Completed,
                _ => TodoStatus::Unspecified,
            },
        })
        .collect()
}

/// Flatten opencode's nested error object (`{name, data:{message}}` or
/// `{message}`) into a display string.
fn opencode_error_message(error: Option<&Value>) -> String {
    let Some(error) = error else {
        return "error".into();
    };
    if let Some(m) = error
        .get("data")
        .and_then(|d| d.get("message"))
        .and_then(Value::as_str)
    {
        return m.to_string();
    }
    if let Some(m) = error.get("message").and_then(Value::as_str) {
        return m.to_string();
    }
    error
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_string()
}

/// opencode tool `content` is an array of `{type:"text", text}` /
/// `{type:"file", …}` blocks. Flatten the text blocks to a display string.
fn opencode_tool_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// `session.next.step.ended` carries per-step `cost`/`tokens`. Surface as a
/// `usage` Custom event so orchestrators get spend visibility (mirrors the
/// claude adapter's `result` → usage Custom).
fn opencode_usage(props: &Value) -> Vec<Payload> {
    if props.get("cost").is_none() && props.get("tokens").is_none() {
        return Vec::new();
    }
    vec![Payload::Custom(Custom {
        name: "usage".into(),
        payload: Some(serde_json::json!({
            "cost": props.get("cost"),
            "tokens": props.get("tokens"),
        })),
    })]
}

fn opencode_permission_desc(p: &Value) -> String {
    // `permission` is an object describing the requested capability; fall back
    // to a stringified form so a consumer always gets *something* to show.
    match p.get("permission") {
        Some(Value::String(s)) => s.clone(),
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

    // ── OpencodeAdapter (serve) ──────────────────────────────────────────
    //
    // Fixtures are the opencode serve SSE shapes (`data: {id,type,properties}`,
    // here as the decoded JSON object). The envelope and the `message.*` /
    // `session.*` / `permission.*` / `todo.*` events were captured from
    // `opencode serve` 1.15.10; the `session.next.*` streaming family is from
    // the same build's live OpenAPI doc (those events only fire on some
    // provider paths). See the OpencodeAdapter doc-comment.
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
        let mut a = PiAdapter::default();
        lines.iter().flat_map(|l| a.parse_line(l)).collect()
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

    mod opencode {
        use super::*;

        fn run(events: &[Value]) -> Vec<Payload> {
            let mut a = OpencodeAdapter::default();
            events.iter().flat_map(|e| a.parse_event(e)).collect()
        }

        #[test]
        fn session_created_maps_to_run_started() {
            let out = run(&[json!({
                "id":"evt_1","type":"session.created",
                "properties":{"sessionID":"ses_a","info":{"id":"ses_a"}}
            })]);
            assert!(matches!(out.as_slice(), [Payload::RunStarted(r)] if r.agent == "opencode"));
        }

        #[test]
        fn session_idle_is_terminal_and_finishes_the_run() {
            let a = OpencodeAdapter::default();
            let idle = json!({"id":"e","type":"session.idle","properties":{"sessionID":"ses_a"}});
            assert!(a.is_terminal(&idle));
            let out = run(&[idle]);
            assert!(matches!(out.as_slice(), [Payload::RunFinished(r)] if r.exit_code == 0));
        }

        #[test]
        fn message_part_text_emits_start_then_only_new_suffix() {
            // opencode re-delivers the whole accumulated text each update; the
            // normalizer must emit one MessageStart and only the new suffix.
            let out = run(&[
                json!({"id":"e1","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_1","messageID":"msg_1","text":"Hel"}}}),
                json!({"id":"e2","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_1","messageID":"msg_1","text":"Hello"}}}),
            ]);
            match out.as_slice() {
                [Payload::MessageStart(s), Payload::MessageDelta(d1), Payload::MessageDelta(d2)] => {
                    assert_eq!(s.role, Role::Assistant);
                    assert_eq!(s.message_id, "prt_1");
                    assert_eq!(d1.text, "Hel");
                    assert_eq!(d2.text, "lo"); // only the new suffix
                }
                other => panic!("expected start + two deltas, got {other:?}"),
            }
        }

        #[test]
        fn real_empty_then_text_snapshot_emits_start_and_one_delta() {
            // The exact sequence captured from opencode 1.15.4 in the runner
            // image for a one-word reply: an empty text part, then the full
            // text. Must emit MessageStart + a single "OK" delta, no blank.
            let out = run(&[
                json!({"id":"e1","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_r","messageID":"msg_r","text":""}}}),
                json!({"id":"e2","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_r","messageID":"msg_r","text":"OK"}}}),
            ]);
            match out.as_slice() {
                [Payload::MessageStart(s), Payload::MessageDelta(d)] => {
                    assert_eq!(s.message_id, "prt_r");
                    assert_eq!(d.text, "OK");
                }
                other => panic!("expected start + one delta, got {other:?}"),
            }
        }

        #[test]
        fn user_prompt_part_is_filtered_out_by_role() {
            // message.updated declares the message's role; a text part of a
            // user message (the echoed prompt) must not become assistant
            // output. Verified ordering: message.updated precedes its parts.
            let out = run(&[
                json!({"id":"e1","type":"message.updated","properties":{
                    "info":{"id":"msg_u","role":"user"}}}),
                json!({"id":"e2","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_u","messageID":"msg_u","text":"my prompt"}}}),
                json!({"id":"e3","type":"message.updated","properties":{
                    "info":{"id":"msg_a","role":"assistant"}}}),
                json!({"id":"e4","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_a","messageID":"msg_a","text":"the reply"}}}),
            ]);
            // Only the assistant part survives.
            match out.as_slice() {
                [Payload::MessageStart(s), Payload::MessageDelta(d)] => {
                    assert_eq!(s.message_id, "prt_a");
                    assert_eq!(d.text, "the reply");
                }
                other => panic!("expected only the assistant reply, got {other:?}"),
            }
        }

        #[test]
        fn message_part_delta_family_is_ignored_to_avoid_double_text() {
            // message.part.delta overlaps message.part.updated for the same
            // partID; consuming it would double the text. Verified ignored.
            let out = run(&[json!({"id":"e","type":"message.part.delta","properties":{
                "sessionID":"s","messageID":"m","partID":"prt_x","field":"text","delta":"The"}})]);
            assert!(out.is_empty());
        }

        #[test]
        fn message_part_redelivered_unchanged_emits_nothing() {
            let out = run(&[
                json!({"id":"e1","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_1","messageID":"msg_1","text":"done"}}}),
                json!({"id":"e2","type":"message.part.updated","properties":{
                    "part":{"type":"text","id":"prt_1","messageID":"msg_1","text":"done"}}}),
            ]);
            // First delivery: start + delta. Second (identical): nothing.
            assert_eq!(out.len(), 2);
        }

        #[test]
        fn tool_part_running_then_completed_pairs_by_call_id() {
            let out = run(&[
                json!({"id":"e1","type":"message.part.updated","properties":{"part":{
                    "type":"tool","callID":"call_1","tool":"bash","id":"prt_2",
                    "state":{"status":"running","input":{"command":"echo HELLO"}}}}}),
                json!({"id":"e2","type":"message.part.updated","properties":{"part":{
                    "type":"tool","callID":"call_1","tool":"bash","id":"prt_2",
                    "state":{"status":"completed","input":{"command":"echo HELLO"},"output":"HELLO"}}}}),
            ]);
            match out.as_slice() {
                [Payload::ToolCall(running), Payload::ToolCall(done)] => {
                    assert_eq!(running.tool_call_id, "call_1");
                    assert_eq!(running.name, "bash");
                    assert_eq!(running.status, ToolStatus::Running);
                    assert_eq!(running.input.as_ref().unwrap()["command"], "echo HELLO");
                    assert_eq!(done.status, ToolStatus::Completed);
                    assert_eq!(done.output, "HELLO");
                }
                other => panic!("expected two ToolCalls, got {other:?}"),
            }
        }

        #[test]
        fn tool_part_same_status_redelivered_is_deduped() {
            let out = run(&[
                json!({"id":"e1","type":"message.part.updated","properties":{"part":{
                    "type":"tool","callID":"c","tool":"read","id":"p","state":{"status":"running"}}}}),
                json!({"id":"e2","type":"message.part.updated","properties":{"part":{
                    "type":"tool","callID":"c","tool":"read","id":"p","state":{"status":"running"}}}}),
            ]);
            assert_eq!(out.len(), 1, "running re-delivered → one event");
        }

        #[test]
        fn tool_part_error_maps_to_error_status() {
            let out = run(&[
                json!({"id":"e","type":"message.part.updated","properties":{"part":{
                "type":"tool","callID":"c","tool":"bash","id":"p",
                "state":{"status":"error","error":"boom"}}}}),
            ]);
            match out.as_slice() {
                [Payload::ToolCall(t)] => {
                    assert_eq!(t.status, ToolStatus::Error);
                    assert_eq!(t.output, "boom");
                }
                other => panic!("expected one ToolCall, got {other:?}"),
            }
        }

        #[test]
        fn session_status_busy_idle_maps_to_phase() {
            let out = run(&[
                json!({"id":"e1","type":"session.status","properties":{"sessionID":"s","status":{"type":"busy"}}}),
                json!({"id":"e2","type":"session.status","properties":{"sessionID":"s","status":{"type":"idle"}}}),
            ]);
            match out.as_slice() {
                [Payload::PhaseChanged(a), Payload::PhaseChanged(b)] => {
                    assert_eq!(a.phase, AgentPhase::Thinking);
                    assert_eq!(b.phase, AgentPhase::WaitingInput);
                }
                other => panic!("expected two PhaseChanged, got {other:?}"),
            }
        }

        #[test]
        fn session_error_becomes_attention_required_with_nested_message() {
            // Real captured shape: error.data.message holds the human reason.
            let out = run(&[json!({"id":"e","type":"session.error","properties":{
                "sessionID":"s",
                "error":{"name":"APIError","data":{"message":"Insufficient Balance","statusCode":402}}}})]);
            match out.as_slice() {
                [Payload::AttentionRequired(a)] => {
                    assert_eq!(a.reason, AttentionReason::ErrorStalled);
                    assert_eq!(a.message, "Insufficient Balance");
                }
                other => panic!("expected AttentionRequired, got {other:?}"),
            }
        }

        #[test]
        fn todo_updated_maps_statuses() {
            let out = run(&[json!({"id":"e","type":"todo.updated","properties":{
                "sessionID":"s","todos":[
                    {"content":"write tests","status":"in_progress","priority":"high"},
                    {"content":"ship it","status":"pending","priority":"low"},
                    {"content":"done thing","status":"completed","priority":"low"}]}})]);
            match out.as_slice() {
                [Payload::TodosUpdated(t)] => {
                    assert_eq!(t.todos.len(), 3);
                    assert_eq!(t.todos[0].text, "write tests");
                    assert_eq!(t.todos[0].status, TodoStatus::InProgress);
                    assert_eq!(t.todos[1].status, TodoStatus::Pending);
                    assert_eq!(t.todos[2].status, TodoStatus::Completed);
                }
                other => panic!("expected TodosUpdated, got {other:?}"),
            }
        }

        #[test]
        fn permission_asked_becomes_permission_requested() {
            let out = run(&[json!({"id":"e","type":"permission.asked","properties":{
                "id":"perm_1","sessionID":"s","tool":"bash",
                "permission":"run shell command","metadata":{"command":"rm -rf /"}}})]);
            match out.as_slice() {
                [Payload::PermissionRequested(p)] => {
                    assert_eq!(p.permission_id, "perm_1");
                    assert_eq!(p.tool, "bash");
                    assert_eq!(p.description, "run shell command");
                    assert_eq!(p.input.as_ref().unwrap()["command"], "rm -rf /");
                }
                other => panic!("expected PermissionRequested, got {other:?}"),
            }
        }

        // ── session.next.* streaming family (schema-verified) ──

        #[test]
        fn next_text_delta_then_ended_makes_start_delta_end() {
            let out = run(&[
                json!({"id":"e1","type":"session.next.text.delta","properties":{
                    "sessionID":"ses_x","timestamp":1,"delta":"hi"}}),
                json!({"id":"e2","type":"session.next.text.ended","properties":{
                    "sessionID":"ses_x","timestamp":2,"text":"hi"}}),
            ]);
            match out.as_slice() {
                [Payload::MessageStart(s), Payload::MessageDelta(d), Payload::MessageEnd(e)] => {
                    assert_eq!(s.role, Role::Assistant);
                    assert_eq!(d.text, "hi");
                    assert_eq!(e.message_id, s.message_id);
                }
                other => panic!("expected start/delta/end, got {other:?}"),
            }
        }

        #[test]
        fn next_tool_called_then_success_pairs_by_call_id() {
            let out = run(&[
                json!({"id":"e1","type":"session.next.tool.called","properties":{
                    "sessionID":"s","timestamp":1,"callID":"c1","tool":"read",
                    "input":{"path":"x"},"provider":"p"}}),
                json!({"id":"e2","type":"session.next.tool.success","properties":{
                    "sessionID":"s","timestamp":2,"callID":"c1","provider":"p",
                    "structured":{},"content":[{"type":"text","text":"file body"}]}}),
            ]);
            match out.as_slice() {
                [Payload::ToolCall(running), Payload::ToolCall(done)] => {
                    assert_eq!(running.name, "read");
                    assert_eq!(running.status, ToolStatus::Running);
                    assert_eq!(done.tool_call_id, "c1");
                    assert_eq!(done.name, "read"); // recovered from state
                    assert_eq!(done.status, ToolStatus::Completed);
                    assert_eq!(done.output, "file body");
                }
                other => panic!("expected two ToolCalls, got {other:?}"),
            }
        }

        #[test]
        fn next_tool_failed_recovers_name_and_flattens_error() {
            let out = run(&[
                json!({"id":"e1","type":"session.next.tool.input.started","properties":{
                    "sessionID":"s","timestamp":1,"callID":"c2","name":"bash"}}),
                json!({"id":"e2","type":"session.next.tool.failed","properties":{
                    "sessionID":"s","timestamp":2,"callID":"c2","provider":"p",
                    "error":{"name":"UnknownError","data":{"message":"nope"}}}}),
            ]);
            match out.as_slice() {
                [Payload::ToolCall(running), Payload::ToolCall(failed)] => {
                    assert_eq!(running.status, ToolStatus::Running);
                    assert_eq!(failed.name, "bash");
                    assert_eq!(failed.status, ToolStatus::Error);
                    assert_eq!(failed.output, "nope");
                }
                other => panic!("expected two ToolCalls, got {other:?}"),
            }
        }

        #[test]
        fn next_step_ended_emits_usage_custom() {
            let out = run(&[
                json!({"id":"e","type":"session.next.step.ended","properties":{
                "sessionID":"s","timestamp":1,"finish":"stop","cost":0.0021,
                "tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}}}}),
            ]);
            match out.as_slice() {
                [Payload::Custom(c)] => {
                    assert_eq!(c.name, "usage");
                    assert_eq!(c.payload.as_ref().unwrap()["tokens"]["input"], 10);
                }
                other => panic!("expected usage Custom, got {other:?}"),
            }
        }

        #[test]
        fn server_connected_and_unknown_events_are_ignored() {
            assert!(run(&[json!({"id":"e","type":"server.connected","properties":{}})]).is_empty());
            assert!(run(&[json!({"id":"e","type":"lsp.updated","properties":{}})]).is_empty());
        }
    }
}
