//! Map `codex app-server`'s JSON-RPC notifications into pillbox §0
//! [`contract::Payload`]s — the codex sibling of [`crate::events::opencode`].
//!
//! `codex app-server` (the channel the Codex VS Code extension drives) speaks
//! **JSON-RPC 2.0 with the `"jsonrpc":"2.0"` header omitted on the wire**, as
//! newline-delimited JSON over stdio. The in-guest [`appserver-host`] bridge
//! (`crate::sandbox::appserver`) owns that stdio pipe, does the `initialize` /
//! `thread/start` handshake, and appends each **notification** line verbatim to
//! a capture file. This module is the pure mapping core the host drains that
//! file through; the NDJSON transport + the bridge live in the run path.
//!
//! ## Verification status
//!
//! Checked against codex 0.160.0 (the runner image's pin) two ways:
//!
//! - **Schema**: every method and field below matches `codex app-server
//!   generate-json-schema` at 0.160.0.
//! - **Live turn**: `tests/fixtures/codex-0.160.0/app-server-turn.ndjson` is a
//!   real 0.160.0 app-server turn captured through the bridge (shell command
//!   writes a file, `apply_patch` adds another, a second command reads them
//!   back, then a streamed reply), with the model replaced by a scripted local
//!   Responses-API endpoint so no credentials were needed. Paths are rewritten
//!   and rate-limit noise dropped; everything else is codex's own output.
//!   `thread/started`, `turn/started`, `item/*` for `userMessage`,
//!   `commandExecution`, `fileChange` and `agentMessage`, the agent-message
//!   deltas, `thread/tokenUsage/updated` and `turn/completed` are therefore
//!   turn-verified. `mcpToolCall`, `dynamicToolCall`, `webSearch`, reasoning
//!   deltas and `error` are schema-verified only.
//!
//! ## The notification envelope
//!
//! A notification is `{"method":"<resource>/<verb>","params":{…}}` (no `id`).
//! The turn streams over these (shapes from `codex app-server
//! generate-json-schema` at codex 0.160.0):
//!
//! - `turn/started` → `{threadId, turn}` — a turn began (→ `Thinking`).
//! - `item/agentMessage/delta` → `{itemId, delta, threadId, turnId}` — assistant
//!   text. First delta for an `itemId` opens the message; subsequent deltas
//!   append.
//! - `item/reasoning/textDelta` / `item/reasoning/summaryTextDelta` →
//!   `{itemId, delta, …}` — reasoning (→ `Thinking`).
//! - `item/started` / `item/completed` → `{item, threadId, turnId, …}` — a
//!   `ThreadItem` (agentMessage / commandExecution / fileChange / mcpToolCall /
//!   …). Tool-shaped items become a `ToolCall`; the agentMessage item closes the
//!   open message.
//! - `thread/tokenUsage/updated` → `{turnId, tokenUsage:{last, total}}` — token
//!   accounting (`last` = the latest model response, `total` = thread-cumulative;
//!   each a `{inputTokens, cachedInputTokens, cacheWriteInputTokens,
//!   outputTokens, reasoningOutputTokens, totalTokens}`). It fires once per
//!   model response, so a tool-using turn sends several and `last` covers only
//!   the final response. The turn's usage is therefore the growth of `total`
//!   since the previous turn's flush, emitted as one §0 `Usage`
//!   (`source: native`) at turn end, so per-turn events are additive. (The `Turn`
//!   in `turn/completed` carries no usage, so this notification is the only
//!   source.)
//! - `turn/completed` → `{threadId, turn}` — the turn went idle (close the open
//!   message + flush the turn's `Usage` + raise the attention signal;
//!   `turn.status == "failed"` → stalled).
//! - `error` → `{error, threadId, turnId, willRetry}` — a turn-level error.
//!
//! Everything else (account/*, thread lifecycle, mcp startup, fuzzyFileSearch,
//! deltas for output we don't surface) is ignored — the stream carries far more
//! than the turn.
//!
//! ## Stateful normalization
//!
//! `item/agentMessage/delta` carries no role and no explicit open/close, so we
//! track the currently-open assistant message id and emit one `MessageStart` on
//! its first delta, closing it on the matching `item/completed` or at
//! `turn/completed`. A tool item's status is emitted only when it *changes*
//! (`item/started` Running → `item/completed` Completed/Error) so the input
//! stream doesn't flood the log with duplicate `ToolCall`s.

use std::collections::HashMap;

use serde_json::Value;

use crate::contract::{
    Actor, AgentPhase, AttentionReason, AttentionRequired, Event, MessageDelta, MessageEnd,
    MessageStart, Payload, PhaseChanged, Role, RunStarted, Thinking, ToolCall, ToolStatus, Usage,
    UsageSource,
};
use crate::events::log::SessionLog;

/// Stateful codex-app-server-notification → §0-payload mapper. One per session
/// stream (the capture file is drained start-to-finish by a single mapper).
#[derive(Default)]
pub(crate) struct CodexServeMapper {
    /// itemId of the currently-open assistant message (set on its first
    /// `item/agentMessage/delta`, cleared when that item completes or the turn
    /// ends). codex streams agentMessage items one at a time (a turn may hold
    /// several), so comparing against this suppresses duplicate
    /// `MessageStart`s without an unbounded seen-set.
    open_msg: Option<String>,
    /// `itemId → last emitted tool status`, so a `ToolCall` is emitted only when
    /// a tool item's status actually changes (started→completed), not on every
    /// re-delivery. Keyed on the mapped status.
    tool_status: HashMap<String, ToolStatus>,
    /// How token updates become the turn's `Usage`.
    accounting: UsageAccounting,
    /// The newest breakdown (with its `turnId`) from `thread/tokenUsage/updated`
    /// not yet accounted: `total` or `last`, per [`UsageAccounting`]. Flushed
    /// at turn end.
    pending_usage: Option<(String, Value)>,
    /// The `total` breakdown already emitted as `Usage`, so each turn's event
    /// carries only that turn's growth and the per-turn events sum to the
    /// thread total.
    accounted_total: Option<Value>,
}

/// How [`CodexServeMapper`] turns `thread/tokenUsage/updated` into a turn's
/// `Usage`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageAccounting {
    /// The growth of the thread-cumulative `total` over the turn, with cache
    /// writes. Correct for any number of model responses per turn (codex-serve).
    #[default]
    ThreadTotal,
    /// The turn's final `last` breakdown without cache writes: the accounting
    /// the sealed execution evidence projection has always used, kept as is.
    /// It counts only the final model response of a multi-response turn.
    LastResponse,
}

impl CodexServeMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_accounting(accounting: UsageAccounting) -> Self {
        Self {
            accounting,
            ..Self::default()
        }
    }

    /// Map one codex app-server **notification** (`{method, params}`) into zero
    /// or more §0 payloads. Unmapped methods (and any responses/requests that
    /// slip in) return empty.
    pub(crate) fn on_notification(&mut self, msg: &Value) -> Vec<Payload> {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let p = msg.get("params").unwrap_or(&Value::Null);
        match method {
            "thread/started" => vec![Payload::RunStarted(RunStarted {
                agent: "codex".into(),
                parent_run_id: String::new(),
                base_snapshot: String::new(),
                requested: None,
            })],
            "turn/started" => vec![Payload::PhaseChanged(PhaseChanged {
                phase: AgentPhase::Thinking,
            })],
            "item/agentMessage/delta" => self.on_agent_delta(p),
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => on_reasoning(p),
            "item/started" => self.on_item(p, false),
            "item/completed" => self.on_item(p, true),
            // Stash usage now; it's emitted as one Usage at turn end (see below).
            "thread/tokenUsage/updated" => {
                self.on_token_usage(p);
                vec![]
            }
            "turn/completed" => self.on_turn_completed(p),
            "error" => self.on_error(p),
            _ => vec![],
        }
    }

    /// `thread/tokenUsage/updated` — codex reports the thread-cumulative usage
    /// (`total`) after every model response, plus that response's own usage
    /// (`last`). Stash the newest `total` (or `last`, for
    /// [`UsageAccounting::LastResponse`]): `last` is not the turn's usage once a
    /// turn makes more than one model call (any tool use), which a live codex
    /// 0.160.0 turn showed.
    fn on_token_usage(&mut self, p: &Value) {
        let key = match self.accounting {
            UsageAccounting::ThreadTotal => "total",
            UsageAccounting::LastResponse => "last",
        };
        let breakdown = p.get("tokenUsage").and_then(|u| u.get(key));
        if let Some(breakdown) = breakdown.filter(|b| b.is_object()) {
            self.pending_usage = Some((str_field(p, "turnId").to_string(), breakdown.clone()));
        }
    }

    /// Emit the turn's usage into `out` — called at every turn-end edge
    /// (`turn/completed` and terminal `error`) so the cost is recorded exactly
    /// once per turn no matter how the turn ends.
    fn flush_usage(&mut self, out: &mut Vec<Payload>) {
        let Some((turn_id, breakdown)) = self.pending_usage.take() else {
            return;
        };
        let turn = match self.accounting {
            UsageAccounting::ThreadTotal => {
                let delta = breakdown_delta(&breakdown, self.accounted_total.as_ref());
                self.accounted_total = Some(breakdown);
                delta
            }
            UsageAccounting::LastResponse => {
                let mut last = breakdown;
                if let Some(fields) = last.as_object_mut() {
                    fields.remove("cacheWriteInputTokens");
                }
                last
            }
        };
        if let Some(usage) = usage_from_breakdown(&turn_id, &turn) {
            out.push(Payload::Usage(usage));
        }
    }

    /// `item/agentMessage/delta` — open the assistant message on the first delta
    /// for its `itemId`, then append. Empty deltas drop.
    fn on_agent_delta(&mut self, p: &Value) -> Vec<Payload> {
        let item_id = str_field(p, "itemId");
        let delta = str_field(p, "delta");
        if item_id.is_empty() || delta.is_empty() {
            return vec![];
        }
        let mut out = Vec::new();
        if self.open_msg.as_deref() != Some(item_id) {
            self.open_msg = Some(item_id.to_string());
            out.push(Payload::MessageStart(MessageStart {
                message_id: item_id.to_string(),
                role: Role::Assistant,
            }));
        }
        out.push(Payload::MessageDelta(MessageDelta {
            message_id: item_id.to_string(),
            text: delta.to_string(),
        }));
        out
    }

    /// `item/started` (`completed = false`) or `item/completed` (`true`) — map
    /// the carried [`ThreadItem`] by its `type`. agentMessage items close the
    /// open message; the tool-shaped items become a `ToolCall`; user/reasoning/
    /// boundary items are handled elsewhere or ignored.
    fn on_item(&mut self, p: &Value, completed: bool) -> Vec<Payload> {
        let item = p.get("item").unwrap_or(&Value::Null);
        let item_type = str_field(item, "type");
        match item_type {
            // The assistant message: deltas already streamed the text, so
            // `started` is a no-op and `completed` just closes the open message.
            "agentMessage" => {
                if !completed {
                    return vec![];
                }
                let id = str_field(item, "id");
                // Close whichever message is open (the completed item's id, or a
                // delta-opened one). If no delta ever arrived (a whole-message
                // item with no streaming), synthesize start+delta from `text`.
                let mut out = Vec::new();
                if self.open_msg.is_none() && !id.is_empty() {
                    let text = str_field(item, "text");
                    out.push(Payload::MessageStart(MessageStart {
                        message_id: id.to_string(),
                        role: Role::Assistant,
                    }));
                    if !text.is_empty() {
                        out.push(Payload::MessageDelta(MessageDelta {
                            message_id: id.to_string(),
                            text: text.to_string(),
                        }));
                    }
                }
                if let Some(open) = self.open_msg.take() {
                    out.push(Payload::MessageEnd(MessageEnd::new(open)));
                } else if !id.is_empty() {
                    out.push(Payload::MessageEnd(MessageEnd::new(id)));
                }
                out
            }
            "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "webSearch"
            | "collabAgentToolCall" => self.on_tool_item(item, item_type, completed),
            // userMessage (our own prompt echo), reasoning (deltas drive it),
            // plan, review-mode, contextCompaction, image* — nothing to surface.
            _ => vec![],
        }
    }

    /// A tool-shaped [`ThreadItem`] → a `ToolCall`, de-duplicated by item id +
    /// mapped status (so `item/started`'s Running and `item/completed`'s
    /// terminal status each emit once, re-deliveries none).
    fn on_tool_item(&mut self, item: &Value, item_type: &str, completed: bool) -> Vec<Payload> {
        let id = str_field(item, "id").to_string();
        if id.is_empty() {
            return vec![];
        }
        // `item/completed` carries the item's terminal `status`; `item/started`
        // has no terminal status yet → Running. Some tool items have no
        // `status` field at all (webSearch), so their completion is the
        // terminal edge on its own.
        let status = match (completed, str_field(item, "status")) {
            (false, _) => ToolStatus::Running,
            (true, "") => ToolStatus::Completed,
            (true, s) => map_item_status(s),
        };
        if self.tool_status.get(&id) == Some(&status) {
            return vec![];
        }
        self.tool_status.insert(id.clone(), status);
        vec![Payload::ToolCall(ToolCall {
            tool_call_id: id,
            name: tool_name(item, item_type),
            status,
            input: tool_input(item, item_type),
            output: if completed {
                tool_output(item, item_type)
            } else {
                String::new()
            },
            title: String::new(),
        })]
    }

    /// `turn/completed` — the turn went idle. Close any open assistant message
    /// and raise the attention signal `wait-idle` keys on. A failed/interrupted
    /// turn raises `ErrorStalled` instead of `NeedsInput`.
    fn on_turn_completed(&mut self, p: &Value) -> Vec<Payload> {
        let mut out = Vec::new();
        if let Some(open) = self.open_msg.take() {
            out.push(Payload::MessageEnd(MessageEnd::new(open)));
        }
        self.flush_usage(&mut out);
        let status = p
            .get("turn")
            .map(|t| str_field(t, "status"))
            .unwrap_or_default();
        let reason = match status {
            "failed" | "interrupted" => AttentionReason::ErrorStalled,
            _ => AttentionReason::NeedsInput,
        };
        out.push(Payload::AttentionRequired(AttentionRequired {
            reason,
            message: String::new(),
        }));
        out
    }

    /// `error` — surface the stall. A **terminal** error (`willRetry == false`)
    /// also closes any open assistant message, so a turn that dies mid-stream
    /// without a following `turn/completed` doesn't leave a dangling `MessageStart`
    /// (an unbalanced §0 stream the next turn would never reconcile). A retriable
    /// error leaves the message open — the same item resumes after reconnect.
    fn on_error(&mut self, p: &Value) -> Vec<Payload> {
        let mut out = Vec::new();
        let terminal = !p.get("willRetry").and_then(Value::as_bool).unwrap_or(false);
        if terminal {
            if let Some(open) = self.open_msg.take() {
                out.push(Payload::MessageEnd(MessageEnd::new(open)));
            }
            self.flush_usage(&mut out); // the turn's over — don't lose a failed turn's cost
        }
        out.push(Payload::AttentionRequired(AttentionRequired {
            reason: AttentionReason::ErrorStalled,
            message: turn_error_message(p.get("error")),
        }));
        out
    }
}

/// Map a reasoning text delta (`item/reasoning/{textDelta,summaryTextDelta}`) to
/// a `Thinking` payload. Empty deltas drop.
fn on_reasoning(p: &Value) -> Vec<Payload> {
    let delta = str_field(p, "delta");
    if delta.is_empty() {
        return vec![];
    }
    vec![Payload::Thinking(Thinking {
        text: delta.to_string(),
    })]
}

/// The component-wise growth of a cumulative `TokenUsageBreakdown` since
/// `previous` (all of `current` when there is none). Counters absent from
/// `current` stay absent, so "not reported" never turns into a zero.
fn breakdown_delta(current: &Value, previous: Option<&Value>) -> Value {
    let mut delta = serde_json::Map::new();
    for key in [
        "inputTokens",
        "cachedInputTokens",
        "cacheWriteInputTokens",
        "outputTokens",
    ] {
        if let Some(now) = current.get(key).and_then(Value::as_u64) {
            let before = previous
                .and_then(|p| p.get(key))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            delta.insert(key.to_string(), now.saturating_sub(before).into());
        }
    }
    Value::Object(delta)
}

/// Map a codex `TokenUsageBreakdown` to a §0 [`Usage`] (`source: native`).
/// codex's `cachedInputTokens` is a SUBSET of `inputTokens` (OpenAI semantics),
/// so split it out into the cache-read field and report only the non-cached
/// remainder as `inputTokens` — matching the non-overlapping Anthropic shape the
/// §0 contract + cost-summer assume (which price cache-read separately, so an
/// overlapping count would double-charge the cached tokens).
/// `cacheWriteInputTokens` (new by codex 0.160.0, serde-defaulted to 0) becomes
/// the cache-creation count; it is mapped exactly as the rollout-transcript
/// parser maps `cache_write_input_tokens`, so the two codex paths agree.
/// `reasoningOutputTokens` is already part of `outputTokens` (a billed subset),
/// so it gets no field of its own. Returns `None` when the breakdown carries no
/// modelled count.
fn usage_from_breakdown(message_id: &str, b: &Value) -> Option<Usage> {
    let n = |k: &str| b.get(k).and_then(Value::as_u64);
    let input_total = n("inputTokens");
    let output = n("outputTokens");
    let cached = n("cachedInputTokens");
    input_total.or(output).or(cached)?;
    Some(Usage {
        message_id: message_id.to_string(),
        input_tokens: input_total.map(|i| i.saturating_sub(cached.unwrap_or(0))),
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: n("cacheWriteInputTokens"),
        cost_usd: None,
        source: UsageSource::Native,
    })
}

/// codex `ThreadItem.status` → §0 `ToolStatus`. Only `item/completed` carries a
/// terminal status; `completed` is success. `failed`, `declined` (an approval
/// was refused: commandExecution/fileChange) and `interrupted` (collab agent
/// calls) are terminal without success, so they are errors. Anything else
/// (`inProgress`) stays Running.
fn map_item_status(s: &str) -> ToolStatus {
    match s {
        "completed" => ToolStatus::Completed,
        "failed" | "declined" | "interrupted" => ToolStatus::Error,
        _ => ToolStatus::Running,
    }
}

/// A display name for a tool item. commandExecution/fileChange have no name
/// field — use the kind; mcp/dynamic tool calls carry a `tool`/`name`.
fn tool_name(item: &Value, item_type: &str) -> String {
    if !matches!(item_type, "mcpToolCall" | "dynamicToolCall") {
        return item_type.to_string();
    }
    // mcp/dynamic calls name the tool in `tool` (or `name`); fall back to the
    // item kind when neither is present.
    for key in ["tool", "name"] {
        let name = str_field(item, key);
        if !name.is_empty() {
            return name.to_string();
        }
    }
    item_type.to_string()
}

/// The tool item's structured input, when the item shape carries one (the
/// command for an exec, the changes for a file edit, the args for an mcp call).
fn tool_input(item: &Value, item_type: &str) -> Option<Value> {
    match item_type {
        "commandExecution" => item.get("command").filter(|v| !v.is_null()).cloned(),
        "fileChange" => item.get("changes").filter(|v| !v.is_null()).cloned(),
        "mcpToolCall" | "dynamicToolCall" => item
            .get("arguments")
            .or_else(|| item.get("input"))
            .filter(|v| !v.is_null())
            .cloned(),
        "webSearch" => item.get("query").filter(|v| !v.is_null()).cloned(),
        _ => None,
    }
}

/// The tool item's output text at completion (the captured command output, the
/// mcp result or error, the dynamic tool's text items). Best-effort: returns ""
/// when the shape carries nothing textual.
fn tool_output(item: &Value, item_type: &str) -> String {
    match item_type {
        "commandExecution" => str_field(item, "aggregatedOutput").to_string(),
        // `result` is `{content, structuredContent, _meta}` on success and null
        // on failure, when `error: {message}` carries the reason instead.
        "mcpToolCall" => match (item.get("result"), item.get("error")) {
            (Some(result @ Value::Object(_)), _) => {
                text_items(result.get("content")).unwrap_or_else(|| result.to_string())
            }
            (Some(Value::String(s)), _) => s.clone(),
            (_, Some(error @ Value::Object(_))) => str_field(error, "message").to_string(),
            _ => String::new(),
        },
        // Dynamic (client-side) tools report `contentItems`, not `result`.
        "dynamicToolCall" => text_items(item.get("contentItems")).unwrap_or_default(),
        _ => match item.get("result").or_else(|| item.get("output")) {
            Some(Value::String(s)) => s.clone(),
            Some(other @ Value::Object(_)) | Some(other @ Value::Array(_)) => other.to_string(),
            _ => String::new(),
        },
    }
}

/// Join the `text` of each content item in an MCP `content` / dynamic-tool
/// `contentItems` array (`{"type":"text"|"inputText","text":…}`; images and
/// audio carry no text). `None` when the value is not an array or holds no text.
fn text_items(items: Option<&Value>) -> Option<String> {
    let texts: Vec<&str> = items?
        .as_array()?
        .iter()
        .filter_map(|it| it.get("text").and_then(Value::as_str))
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// Flatten a codex `TurnError` (`{message}` or a bare string) to a display line.
fn turn_error_message(error: Option<&Value>) -> String {
    match error {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string(),
        _ => "error".to_string(),
    }
}

/// Borrow a string field as `&str`, or `""`.
fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Drain a codex app-server **NDJSON** capture (one JSON message per line) into
/// the durable [`SessionLog`], mapping each notification through
/// [`CodexServeMapper`] — the codex analog of [`drain_sse`](crate::events::opencode::drain_sse),
/// minus the SSE framing (codex's wire is already line-delimited JSON, so a line
/// *is* a message).
///
/// The in-guest [`appserver-host`](crate::sandbox::appserver) bridge appends
/// each notification line to the capture file; the host drains it (replay +
/// follow) on `watch`/`subscribe`/`ingest`, exactly like opencode's `/event`
/// file. Pass a [`FollowReader`](crate::events::opencode::FollowReader) for a
/// live session (tails appends) or a plain `File` for a post-hoc drain (reads to
/// EOF). Blocks until the reader ends or `stop` is set (observed between lines).
/// Non-JSON lines and unmapped methods are skipped, not errored, so a stray line
/// can't wedge the drain. Returns the number of §0 events appended.
pub(crate) fn drain_ndjson<R: std::io::Read>(
    reader: R,
    session_id: &str,
    log: &mut SessionLog,
    stop: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<usize> {
    use std::io::BufRead as _;
    use std::sync::atomic::Ordering;

    let mut mapper = CodexServeMapper::new();
    let mut total = 0;
    let mut lines = std::io::BufReader::new(reader).lines();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(line) = lines.next() else { break };
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // codex app-server notifications are the agent's own output — stamp `agent`.
        let events: Vec<Event> = mapper
            .on_notification(&value)
            .into_iter()
            .map(|p| Event::session(session_id, p).with_actor(Actor::agent("codex")))
            .collect();
        if !events.is_empty() {
            total += events.len();
            log.append(&events)?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Inline fixtures are the codex app-server notification shapes
    // (`{method, params}`, `"jsonrpc"` omitted on the wire) from
    // `codex app-server generate-json-schema` at codex 0.160.0;
    // `maps_a_live_codex_0_160_0_turn` replays a real captured turn.
    fn run(msgs: &[Value]) -> Vec<Payload> {
        let mut m = CodexServeMapper::new();
        msgs.iter().flat_map(|e| m.on_notification(e)).collect()
    }

    #[test]
    fn thread_started_maps_to_run_started() {
        let out = run(&[json!({
            "method":"thread/started",
            "params":{"thread":{"id":"th_1","status":"idle"}}
        })]);
        assert!(matches!(out.as_slice(), [Payload::RunStarted(r)] if r.agent == "codex"));
    }

    #[test]
    fn turn_started_maps_to_thinking_phase() {
        let out = run(&[json!({
            "method":"turn/started",
            "params":{"threadId":"th_1","turn":{"id":"tu_1","status":"inProgress","items":[]}}
        })]);
        assert!(
            matches!(out.as_slice(), [Payload::PhaseChanged(p)] if p.phase == AgentPhase::Thinking)
        );
    }

    #[test]
    fn agent_message_delta_opens_once_then_appends() {
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_msg","delta":"Hel","threadId":"th","turnId":"tu"}}),
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_msg","delta":"lo","threadId":"th","turnId":"tu"}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(s), Payload::MessageDelta(d1), Payload::MessageDelta(d2)] => {
                assert_eq!(s.role, Role::Assistant);
                assert_eq!(s.message_id, "it_msg");
                assert_eq!(d1.text, "Hel");
                assert_eq!(d2.text, "lo");
                assert_eq!(d2.message_id, "it_msg");
            }
            other => panic!("expected start + two deltas, got {other:?}"),
        }
    }

    #[test]
    fn agent_message_completed_closes_the_open_message() {
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_msg","delta":"hi","threadId":"th","turnId":"tu"}}),
            json!({"method":"item/completed","params":{
                "threadId":"th","turnId":"tu","completedAtMs":1,
                "item":{"type":"agentMessage","id":"it_msg","text":"hi"}}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(_), Payload::MessageDelta(_), Payload::MessageEnd(e)] => {
                assert_eq!(e.message_id, "it_msg");
            }
            other => panic!("expected start/delta/end, got {other:?}"),
        }
    }

    #[test]
    fn agent_message_item_without_deltas_synthesizes_whole_message() {
        // A completed agentMessage with no preceding deltas (non-streaming path):
        // must still surface start + the full text + end.
        let out = run(&[json!({"method":"item/completed","params":{
            "threadId":"th","turnId":"tu","completedAtMs":1,
            "item":{"type":"agentMessage","id":"it_x","text":"the whole reply"}}})]);
        match out.as_slice() {
            [Payload::MessageStart(s), Payload::MessageDelta(d), Payload::MessageEnd(e)] => {
                assert_eq!(s.message_id, "it_x");
                assert_eq!(d.text, "the whole reply");
                assert_eq!(e.message_id, "it_x");
            }
            other => panic!("expected start/delta/end, got {other:?}"),
        }
    }

    #[test]
    fn reasoning_delta_maps_to_thinking() {
        let out = run(&[json!({"method":"item/reasoning/textDelta","params":{
            "itemId":"it_r","contentIndex":0,"delta":"let me think","threadId":"th","turnId":"tu"}})]);
        assert!(matches!(out.as_slice(), [Payload::Thinking(t)] if t.text == "let me think"));
    }

    #[test]
    fn command_execution_started_then_completed_pairs_by_id() {
        let out = run(&[
            json!({"method":"item/started","params":{
                "threadId":"th","turnId":"tu","startedAtMs":1,
                "item":{"type":"commandExecution","id":"it_c","command":"echo HELLO",
                        "commandActions":[],"cwd":"/workspace","status":"inProgress"}}}),
            json!({"method":"item/completed","params":{
                "threadId":"th","turnId":"tu","completedAtMs":2,
                "item":{"type":"commandExecution","id":"it_c","command":"echo HELLO",
                        "commandActions":[],"cwd":"/workspace","status":"completed",
                        "exitCode":0,"aggregatedOutput":"HELLO\n"}}}),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(running), Payload::ToolCall(done)] => {
                assert_eq!(running.tool_call_id, "it_c");
                assert_eq!(running.name, "commandExecution");
                assert_eq!(running.status, ToolStatus::Running);
                assert_eq!(running.input.as_ref().unwrap(), "echo HELLO");
                assert_eq!(done.status, ToolStatus::Completed);
                assert_eq!(done.output, "HELLO\n");
            }
            other => panic!("expected two ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn command_execution_failed_maps_to_error_status() {
        let out = run(&[json!({"method":"item/completed","params":{
            "threadId":"th","turnId":"tu","completedAtMs":2,
            "item":{"type":"commandExecution","id":"it_f","command":"false",
                    "commandActions":[],"cwd":"/w","status":"failed","exitCode":1,
                    "aggregatedOutput":"boom"}}})]);
        match out.as_slice() {
            [Payload::ToolCall(t)] => {
                assert_eq!(t.status, ToolStatus::Error);
                assert_eq!(t.output, "boom");
            }
            other => panic!("expected one ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_item_same_status_redelivered_is_deduped() {
        let out = run(&[
            json!({"method":"item/started","params":{"threadId":"t","turnId":"u","startedAtMs":1,
                "item":{"type":"commandExecution","id":"c","command":"x","commandActions":[],
                        "cwd":"/w","status":"inProgress"}}}),
            json!({"method":"item/started","params":{"threadId":"t","turnId":"u","startedAtMs":1,
                "item":{"type":"commandExecution","id":"c","command":"x","commandActions":[],
                        "cwd":"/w","status":"inProgress"}}}),
        ]);
        assert_eq!(out.len(), 1, "running re-delivered → one event");
    }

    #[test]
    fn mcp_tool_call_uses_tool_name() {
        let out = run(&[json!({"method":"item/completed","params":{
            "threadId":"t","turnId":"u","completedAtMs":2,
            "item":{"type":"mcpToolCall","id":"m1","tool":"search","status":"completed",
                    "arguments":{"q":"rust"},"result":"hit"}}})]);
        match out.as_slice() {
            [Payload::ToolCall(t)] => {
                assert_eq!(t.name, "search");
                assert_eq!(t.input.as_ref().unwrap()["q"], "rust");
                assert_eq!(t.output, "hit");
            }
            other => panic!("expected one ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn turn_completed_closes_message_and_signals_idle() {
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_m","delta":"done","threadId":"th","turnId":"tu"}}),
            json!({"method":"turn/completed","params":{
                "threadId":"th","turn":{"id":"tu","status":"completed","items":[]}}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(_), Payload::MessageDelta(_), Payload::MessageEnd(e), Payload::AttentionRequired(a)] =>
            {
                assert_eq!(e.message_id, "it_m");
                assert_eq!(a.reason, AttentionReason::NeedsInput);
            }
            other => panic!("expected start/delta/end + idle attention, got {other:?}"),
        }
    }

    fn token_update(turn: &str, input: u64, cached: u64, write: Option<u64>, out: u64) -> Value {
        let mut total = json!({"inputTokens":input,"cachedInputTokens":cached,
            "outputTokens":out,"reasoningOutputTokens":0,"totalTokens":input + out});
        if let Some(write) = write {
            total["cacheWriteInputTokens"] = write.into();
        }
        json!({"method":"thread/tokenUsage/updated","params":{
            "threadId":"th","turnId":turn,"tokenUsage":{
                // `last` is one model response; the mapper must not use it as
                // the turn's usage.
                "last":{"inputTokens":1,"cachedInputTokens":0,"outputTokens":1,
                        "reasoningOutputTokens":0,"totalTokens":2},
                "total":total}}})
    }

    fn turn_completed(turn: &str) -> Value {
        json!({"method":"turn/completed","params":{
            "threadId":"th","turn":{"id":turn,"status":"completed","items":[]}}})
    }

    fn usages(out: &[Payload]) -> Vec<&Usage> {
        out.iter()
            .filter_map(|p| match p {
                Payload::Usage(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn token_usage_flushes_one_usage_at_turn_end() {
        // cachedInputTokens is a SUBSET of inputTokens, so the mapped input is
        // the non-cached remainder (1000-200) and cache-read is the cached 200;
        // reasoning is folded into output upstream.
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_m","delta":"hi","threadId":"th","turnId":"tu"}}),
            token_update("tu", 1000, 200, None, 50),
            turn_completed("tu"),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(_), Payload::MessageDelta(_), Payload::MessageEnd(_), Payload::Usage(u), Payload::AttentionRequired(a)] =>
            {
                assert_eq!(u.message_id, "tu");
                assert_eq!(u.input_tokens, Some(800)); // 1000 - 200 cached
                assert_eq!(u.cache_read_input_tokens, Some(200));
                assert_eq!(u.output_tokens, Some(50));
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.source, UsageSource::Native);
                assert_eq!(a.reason, AttentionReason::NeedsInput);
            }
            other => panic!("expected end + usage + idle, got {other:?}"),
        }
    }

    #[test]
    fn multi_response_turns_account_the_growth_of_the_thread_total() {
        // codex 0.160.0 sends one update per model response; a tool-using turn
        // sends several. Each turn's Usage is the growth of `total`, so the
        // per-turn events sum to the thread total.
        let out = run(&[
            token_update("t1", 100, 40, Some(10), 20),
            token_update("t1", 200, 80, Some(20), 40),
            token_update("t1", 300, 120, Some(30), 60),
            turn_completed("t1"),
            token_update("t2", 450, 150, Some(30), 90),
            turn_completed("t2"),
        ]);
        let u = usages(&out);
        assert_eq!(u.len(), 2, "one Usage per turn: {out:?}");
        assert_eq!(u[0].message_id, "t1");
        assert_eq!(u[0].input_tokens, Some(180)); // 300 - 120 cached
        assert_eq!(u[0].cache_read_input_tokens, Some(120));
        assert_eq!(u[0].cache_creation_input_tokens, Some(30));
        assert_eq!(u[0].output_tokens, Some(60));
        assert_eq!(u[1].message_id, "t2");
        assert_eq!(u[1].input_tokens, Some(120)); // (450-300) - (150-120)
        assert_eq!(u[1].cache_read_input_tokens, Some(30));
        assert_eq!(u[1].cache_creation_input_tokens, Some(0));
        assert_eq!(u[1].output_tokens, Some(30));
    }

    #[test]
    fn last_response_accounting_keeps_the_evidence_projection_unchanged() {
        let mut m = CodexServeMapper::with_accounting(UsageAccounting::LastResponse);
        let mut update = token_update("t1", 300, 120, Some(30), 60);
        update["params"]["tokenUsage"]["last"] = json!({"inputTokens":9,
            "cachedInputTokens":2,"cacheWriteInputTokens":4,"outputTokens":3,
            "reasoningOutputTokens":1,"totalTokens":12});
        let out: Vec<Payload> = [update, turn_completed("t1")]
            .iter()
            .flat_map(|e| m.on_notification(e))
            .collect();
        let u = usages(&out);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].input_tokens, Some(7));
        assert_eq!(u[0].cache_read_input_tokens, Some(2));
        assert_eq!(u[0].cache_creation_input_tokens, None);
        assert_eq!(u[0].output_tokens, Some(3));
    }

    #[test]
    fn declined_and_status_less_tool_items_reach_a_terminal_status() {
        let out = run(&[
            // An approval refused → `declined`, terminal without success.
            json!({"method":"item/completed","params":{"threadId":"t","turnId":"u",
                "completedAtMs":2,"item":{"type":"fileChange","id":"f","changes":[],
                "status":"declined"}}}),
            // webSearch items carry no `status`; completion is the terminal edge.
            json!({"method":"item/started","params":{"threadId":"t","turnId":"u",
                "startedAtMs":1,"item":{"type":"webSearch","id":"w","query":"rust"}}}),
            json!({"method":"item/completed","params":{"threadId":"t","turnId":"u",
                "completedAtMs":2,"item":{"type":"webSearch","id":"w","query":"rust"}}}),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(f), Payload::ToolCall(w1), Payload::ToolCall(w2)] => {
                assert_eq!(f.status, ToolStatus::Error);
                assert_eq!(w1.status, ToolStatus::Running);
                assert_eq!(w2.status, ToolStatus::Completed);
            }
            other => panic!("expected three ToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn mcp_and_dynamic_tool_outputs_use_the_0_160_0_shapes() {
        let out = run(&[
            json!({"method":"item/completed","params":{"threadId":"t","turnId":"u",
                "completedAtMs":2,"item":{"type":"mcpToolCall","id":"m1","server":"s",
                "tool":"search","status":"completed","arguments":{},
                "result":{"content":[{"type":"text","text":"hit"}],
                          "structuredContent":null,"_meta":null},"error":null}}}),
            json!({"method":"item/completed","params":{"threadId":"t","turnId":"u",
                "completedAtMs":2,"item":{"type":"mcpToolCall","id":"m2","server":"s",
                "tool":"search","status":"failed","arguments":{},"result":null,
                "error":{"message":"server unavailable"}}}}),
            json!({"method":"item/completed","params":{"threadId":"t","turnId":"u",
                "completedAtMs":2,"item":{"type":"dynamicToolCall","id":"d1","tool":"lookup",
                "arguments":{"k":1},"status":"completed","success":true,
                "contentItems":[{"type":"inputText","text":"found"},
                                {"type":"inputImage","imageUrl":"data:"}]}}}),
        ]);
        match out.as_slice() {
            [Payload::ToolCall(ok), Payload::ToolCall(failed), Payload::ToolCall(dynamic)] => {
                assert_eq!(ok.output, "hit");
                assert_eq!(failed.status, ToolStatus::Error);
                assert_eq!(failed.output, "server unavailable");
                assert_eq!(dynamic.name, "lookup");
                assert_eq!(dynamic.output, "found");
            }
            other => panic!("expected three ToolCalls, got {other:?}"),
        }
    }

    /// A real codex 0.160.0 app-server turn captured through the bridge: a
    /// shell command writes hello.txt, apply_patch adds notes.md, a second
    /// command reads them back, then a streamed reply. Four model responses, so
    /// four `thread/tokenUsage/updated` notifications in one turn.
    #[test]
    fn maps_a_live_codex_0_160_0_turn() {
        let capture = include_str!("../../tests/fixtures/codex-0.160.0/app-server-turn.ndjson");
        let msgs: Vec<Value> = capture
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let out = run(&msgs);

        assert!(matches!(out.first(), Some(Payload::RunStarted(r)) if r.agent == "codex"));
        let tools: Vec<&ToolCall> = out
            .iter()
            .filter_map(|p| match p {
                Payload::ToolCall(t) => Some(t),
                _ => None,
            })
            .collect();
        let done: Vec<(&str, &str)> = tools
            .iter()
            .filter(|t| t.status == ToolStatus::Completed)
            .map(|t| (t.name.as_str(), t.tool_call_id.as_str()))
            .collect();
        assert_eq!(
            done,
            [
                ("commandExecution", "call_5"),
                ("fileChange", "call_6"),
                ("commandExecution", "call_7"),
            ]
        );
        assert_eq!(tools.len(), 6, "each tool item: Running then Completed");
        let read_back = tools.iter().rfind(|t| t.tool_call_id == "call_7").unwrap();
        assert_eq!(read_back.output, "hello from codex\nhello.txt\nnotes.md\n");
        let patch = tools.iter().rfind(|t| t.tool_call_id == "call_6").unwrap();
        assert_eq!(
            patch.input.as_ref().unwrap()[0]["path"],
            "/workspace/demo/notes.md"
        );

        let text: String = out
            .iter()
            .filter_map(|p| match p {
                Payload::MessageDelta(d) => Some(d.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Wrote hello.txt and listed the directory.");
        assert_eq!(
            out.iter()
                .filter(|p| matches!(p, Payload::MessageStart(_)))
                .count(),
            1
        );
        assert_eq!(
            out.iter()
                .filter(|p| matches!(p, Payload::MessageEnd(_)))
                .count(),
            1
        );

        // The whole turn's usage: the final `total` (input 426 of which 160
        // cached and 40 cache writes, output 80), not the last response's.
        let u = usages(&out);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].input_tokens, Some(266));
        assert_eq!(u[0].cache_read_input_tokens, Some(160));
        assert_eq!(u[0].cache_creation_input_tokens, Some(40));
        assert_eq!(u[0].output_tokens, Some(80));

        assert!(matches!(
            out.last(),
            Some(Payload::AttentionRequired(a)) if a.reason == AttentionReason::NeedsInput
        ));
    }

    #[test]
    fn turn_completed_failed_raises_error_stalled() {
        let out = run(&[json!({"method":"turn/completed","params":{
            "threadId":"th","turn":{"id":"tu","status":"failed","items":[]}}})]);
        assert!(matches!(
            out.as_slice(),
            [Payload::AttentionRequired(a)] if a.reason == AttentionReason::ErrorStalled
        ));
    }

    #[test]
    fn error_notification_becomes_attention_with_message() {
        let out = run(&[json!({"method":"error","params":{
            "threadId":"th","turnId":"tu","willRetry":false,
            "error":{"message":"model overloaded"}}})]);
        match out.as_slice() {
            [Payload::AttentionRequired(a)] => {
                assert_eq!(a.reason, AttentionReason::ErrorStalled);
                assert_eq!(a.message, "model overloaded");
            }
            other => panic!("expected AttentionRequired, got {other:?}"),
        }
    }

    #[test]
    fn terminal_error_closes_open_message_retriable_leaves_it() {
        // A retriable error mid-stream must NOT close the message (the item
        // resumes after reconnect) — just the stall signal.
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_e","delta":"par","threadId":"th","turnId":"tu"}}),
            json!({"method":"error","params":{
                "threadId":"th","turnId":"tu","willRetry":true,
                "error":{"message":"Reconnecting... 1/5"}}}),
        ]);
        assert!(
            !out.iter().any(|p| matches!(p, Payload::MessageEnd(_))),
            "retriable error must not close the message: {out:?}"
        );

        // A terminal error closes the dangling message so the §0 stream balances.
        let out = run(&[
            json!({"method":"item/agentMessage/delta","params":{
                "itemId":"it_t","delta":"half","threadId":"th","turnId":"tu"}}),
            json!({"method":"error","params":{
                "threadId":"th","turnId":"tu","willRetry":false,
                "error":{"message":"fatal"}}}),
        ]);
        match out.as_slice() {
            [Payload::MessageStart(_), Payload::MessageDelta(_), Payload::MessageEnd(e), Payload::AttentionRequired(a)] =>
            {
                assert_eq!(e.message_id, "it_t");
                assert_eq!(a.reason, AttentionReason::ErrorStalled);
            }
            other => panic!("expected start/delta/end + stall, got {other:?}"),
        }
    }

    #[test]
    fn drain_ndjson_feeds_the_durable_log() {
        use std::io::Cursor;
        use std::sync::atomic::AtomicBool;

        crate::test_util::with_isolated_home("codex-serve-drain", || {
            let g = crate::pillbox::global();
            let mut log = SessionLog::open(&g, "ses-cx").expect("open log");
            // A minimal turn: thread start, one streamed message, idle.
            let stream = [
                r#"{"method":"thread/started","params":{"thread":{"id":"th","status":"idle"}}}"#,
                r#"{"method":"item/agentMessage/delta","params":{"itemId":"m","delta":"hi","threadId":"th","turnId":"tu"}}"#,
                r#"{"id":1,"result":{"turn":{"id":"tu"}}}"#,
                r#"not json — must be skipped, not fatal"#,
                r#"{"method":"turn/completed","params":{"threadId":"th","turn":{"id":"tu","status":"completed","items":[]}}}"#,
            ]
            .join("\n");
            let stop = AtomicBool::new(false);
            let n = drain_ndjson(Cursor::new(stream), "ses-cx", &mut log, &stop).expect("drain");
            // RunStarted, MessageStart, MessageDelta, MessageEnd, AttentionRequired = 5.
            assert_eq!(n, 5, "expected 5 mapped §0 events");
            // Every drained event is stamped as the codex agent.
            let events = SessionLog::open(&g, "ses-cx")
                .unwrap()
                .read_from(0)
                .unwrap();
            assert!(events
                .iter()
                .all(|e| e.actor == Some(crate::contract::Actor::agent("codex"))));
        });
    }

    #[test]
    fn unknown_and_lifecycle_methods_are_ignored() {
        assert!(run(&[json!({"method":"account/updated","params":{}})]).is_empty());
        assert!(run(&[json!({"method":"thread/tokenUsage/updated","params":{}})]).is_empty());
        assert!(run(&[json!({"method":"mcpServer/startupStatus/updated","params":{}})]).is_empty());
        // A response (has `id`+`result`, no `method`) must not map to anything.
        assert!(run(&[json!({"id":7,"result":{"turn":{"id":"tu"}}})]).is_empty());
    }
}
