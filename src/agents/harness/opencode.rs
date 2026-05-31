//! opencode harness adapter — `opencode serve` (a `ServeAdapter`, HTTP+SSE).
//! See the `OpencodeAdapter` doc for schema sources / verification status.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::contract::{
    AgentPhase, AttentionReason, AttentionRequired, Custom, MessageDelta, MessageEnd, MessageStart,
    Payload, PermissionRequested, PhaseChanged, Role, RunFinished, RunStarted, Todo, TodoStatus,
    TodosUpdated, ToolCall, ToolStatus,
};

use super::{str_field, ServeAdapter};

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
                out.push(Payload::MessageEnd(MessageEnd::new(mid)));
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

    // ── OpencodeAdapter (serve) ──────────────────────────────────────────
    //
    // Fixtures are the opencode serve SSE shapes (`data: {id,type,properties}`,
    // here as the decoded JSON object). The envelope and the `message.*` /
    // `session.*` / `permission.*` / `todo.*` events were captured from
    // `opencode serve` 1.15.10; the `session.next.*` streaming family is from
    // the same build's live OpenAPI doc (those events only fire on some
    // provider paths). See the OpencodeAdapter doc-comment.
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
