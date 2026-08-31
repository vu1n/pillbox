//! Map opencode's `/event` SSE envelopes into pillbox §0 `contract::Payload`s.
//!
//! opencode is **structured-API-native**: `opencode serve` exposes a headless
//! HTTP server whose `/event` endpoint streams typed events, so instead of
//! scraping a transcript file (the claude/codex `transcripts` path) we consume
//! its event stream directly — structured, real-time, no file-tailing. This
//! module is the pure mapping core; the SSE transport + the bridge that feeds
//! the durable [`SessionLog`](crate::events::log::SessionLog) live in the
//! sandbox run path.
//!
//! ## Which events carry the turn
//!
//! Verified against a live GLM turn through `opencode serve` (not just the
//! OpenAPI): the assistant turn streams over the **`message.*` family** —
//!
//! - `message.updated` → `info:{id, role, model, structured?}` — a message was
//!   created / updated. The first sight of an `assistant` message id opens it;
//!   a final schema-bound value is projected once into `MessageDelta` evidence.
//! - `message.part.delta` → `{messageID, field, delta}` — incremental content
//!   (`field:"text"` = assistant text; `field:"reasoning"` = thinking).
//! - `message.part.updated` with `part.type == "tool"` → `{tool, callID,
//!   state:{status, input, output}}` — a tool call's evolving state.
//! - `message.part.updated` with `part.type == "step-finish"` → `{messageID,
//!   tokens:{input, output, cache:{read, write}}}` — a finished model step's
//!   token accounting, mapped to a §0 `Usage` (`source: native`). This is the
//!   live-verified token source: `message.updated`'s `info` carries no tokens,
//!   so the turn's cost lands here or nowhere.
//! - `session.idle` → the turn went quiescent (end the open message + the
//!   `NeedsInput` attention signal, matching the claude end_turn producer).
//!
//! The parallel `session.next.*` family exists in the OpenAPI but only emitted
//! lifecycle bits (`agent.switched`, `model.switched`) in practice — it is
//! *not* the content source, so we ignore it (along with the `text`/`reasoning`
//! part *snapshots*, whose content the deltas already carry, and `step-start`,
//! `session.{updated,status,diff}`, `server.*`).
//!
//! Stateful: deltas carry a `messageID` but no role, so we track which message
//! ids we've opened as assistant (emit `MessageStart` once each); a tool's
//! status is emitted only when it *changes* (`pending`→`running`→`completed`)
//! so a chatty input-stream doesn't flood the log with duplicate `ToolCall`s.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::contract::{
    Actor, AttentionReason, AttentionRequired, Event, MessageDelta, MessageEnd, MessageStart,
    Payload, Role, Thinking, ToolCall, ToolStatus, Usage, UsageSource,
};
use crate::events::log::SessionLog;

/// Stateful opencode-event → §0-payload mapper. One per session stream.
#[derive(Default)]
pub(crate) struct EventMapper {
    /// The currently-open assistant message id (set on the first `message.updated`
    /// for an assistant message, cleared on `session.idle`). `message.updated`
    /// fires repeatedly for the same message; comparing against this suppresses
    /// duplicate `MessageStart`s without an ever-growing seen-set, since opencode
    /// opens exactly one assistant message per turn (a new id only after idle).
    open_msg: Option<String>,
    /// The assistant message whose final schema-bound value was projected into
    /// the MessageDelta evidence channel. Cleared when the turn goes idle.
    structured_msg: Option<String>,
    /// `callID → last emitted tool status`, so we only emit a `ToolCall` when a
    /// tool's status actually changes, not on every input-stream tick. Keyed on
    /// the *mapped* status so opencode's `pending`→`running` (both `Running`)
    /// collapses to one event.
    tool_status: HashMap<String, ToolStatus>,
    /// Step part ids whose `step-finish` usage we've already emitted, so a
    /// re-sent `part.updated` for the same step can't double-count tokens.
    steps_seen: HashSet<String>,
}

impl EventMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Map one opencode `/event` envelope into zero or more §0 payloads.
    /// Unmapped types return empty (the stream carries far more than the turn —
    /// lifecycle, sync, tui, lsp, step boundaries, …).
    pub(crate) fn on_event(&mut self, ev: &Value) -> Vec<Payload> {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or_default();
        let p = ev.get("properties").unwrap_or(&Value::Null);

        match ty {
            "message.updated" => self.on_message_updated(p),
            "message.part.delta" => self.on_part_delta(p),
            "message.part.updated" => self.on_part_updated(p),
            // Turn went quiescent → close the open assistant message and raise
            // the attention signal the driver waits on.
            "session.idle" => {
                let mut out = Vec::new();
                if let Some(id) = self.open_msg.take() {
                    out.push(Payload::MessageEnd(MessageEnd::new(id)));
                }
                self.structured_msg = None;
                out.push(attention(AttentionReason::NeedsInput));
                out
            }
            "permission.asked" => vec![attention(AttentionReason::Permission)],
            "question.asked" => vec![attention(AttentionReason::NeedsInput)],
            "session.error" => vec![Payload::AttentionRequired(AttentionRequired {
                reason: AttentionReason::ErrorStalled,
                message: error_message(p),
            })],
            _ => vec![],
        }
    }

    /// `message.updated` — open an assistant message on its first sighting and
    /// project OpenCode's final schema-bound value into the text evidence
    /// channel. User messages and repeats without new structured output produce
    /// nothing.
    fn on_message_updated(&mut self, p: &Value) -> Vec<Payload> {
        let info = p.get("info").unwrap_or(&Value::Null);
        let role = info.get("role").and_then(Value::as_str).unwrap_or_default();
        let id = info.get("id").and_then(Value::as_str).unwrap_or_default();
        if role != "assistant" || id.is_empty() {
            return vec![];
        }
        let mut out = Vec::new();
        if self.open_msg.as_deref() != Some(id) {
            self.open_msg = Some(id.to_string());
            out.push(Payload::MessageStart(MessageStart {
                message_id: id.to_string(),
                role: Role::Assistant,
            }));
        }
        if self.structured_msg.as_deref() != Some(id) {
            if let Some(structured) = info.get("structured") {
                self.structured_msg = Some(id.to_string());
                out.push(Payload::MessageDelta(MessageDelta {
                    message_id: id.to_string(),
                    text: structured.to_string(),
                }));
            }
        }
        out
    }

    /// `message.part.delta` — the streaming content. `field` selects the §0
    /// channel: assistant text vs. reasoning/thinking. Empty deltas drop.
    fn on_part_delta(&mut self, p: &Value) -> Vec<Payload> {
        let delta = p.get("delta").and_then(Value::as_str).unwrap_or_default();
        if delta.is_empty() {
            return vec![];
        }
        match p.get("field").and_then(Value::as_str).unwrap_or("text") {
            "reasoning" => vec![Payload::Thinking(Thinking {
                text: delta.to_string(),
            })],
            // Default to text (the common case; opencode's deltas are `text`).
            _ => {
                // Attach to the delta's own messageID, falling back to the open
                // assistant message. If neither exists there's nothing to attach
                // to — drop it rather than emit a delta with an empty id.
                let Some(message_id) = p
                    .get("messageID")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.open_msg.clone())
                else {
                    return vec![];
                };
                vec![Payload::MessageDelta(MessageDelta {
                    message_id,
                    text: delta.to_string(),
                })]
            }
        }
    }

    /// `message.part.updated` — two part kinds carry the turn: `tool` (a tool
    /// call's evolving state) and `step-finish` (a model step's token
    /// accounting). Other parts (text/reasoning snapshots duplicate the deltas;
    /// `step-start` is a boundary) produce nothing.
    fn on_part_updated(&mut self, p: &Value) -> Vec<Payload> {
        let part = p.get("part").unwrap_or(&Value::Null);
        match part.get("type").and_then(Value::as_str) {
            Some("tool") => self.on_tool_part(part),
            Some("step-finish") => self.on_step_finish(part),
            _ => vec![],
        }
    }

    /// A `tool` part. Emits a `ToolCall` only when the tool's status changes.
    fn on_tool_part(&mut self, part: &Value) -> Vec<Payload> {
        let call_id = part
            .get("callID")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let state = part.get("state").unwrap_or(&Value::Null);
        let status = map_tool_status(
            state
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("running"),
        );
        // De-dupe: a tool part updates repeatedly as its input streams; only the
        // mapped-status transitions (Running → Completed/Error) are interesting.
        if self.tool_status.get(&call_id) == Some(&status) {
            return vec![];
        }
        self.tool_status.insert(call_id.clone(), status);
        vec![Payload::ToolCall(ToolCall {
            tool_call_id: call_id,
            name: part
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            status,
            input: state.get("input").filter(|v| !v.is_null()).cloned(),
            output: state
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            title: String::new(),
        })]
    }

    /// A finished model step reports its token usage. Emit one §0 `Usage`
    /// (`source: native`) per step, de-duped on the step part id so a re-sent
    /// `part.updated` doesn't double-count. A step with no modelled token field
    /// (e.g. a `{total}`-only shape) yields nothing.
    fn on_step_finish(&mut self, part: &Value) -> Vec<Payload> {
        let Some(usage) = usage_from_step(part) else {
            return vec![];
        };
        if let Some(id) = part.get("id").and_then(Value::as_str) {
            if !self.steps_seen.insert(id.to_string()) {
                return vec![];
            }
        }
        vec![Payload::Usage(usage)]
    }
}

/// Map an opencode `step-finish` part's `tokens` into a §0 [`Usage`]
/// (`source: native`, mirroring the transcript producer). Returns `None` when
/// none of the modelled token fields are present, so a `{total}`-only or
/// token-less step produces no event rather than an all-`None` `Usage`.
fn usage_from_step(part: &Value) -> Option<Usage> {
    let tokens = part.get("tokens")?;
    let count = |obj: &Value, k: &str| obj.get(k).and_then(Value::as_u64);
    let cache = tokens.get("cache").unwrap_or(&Value::Null);
    let input = count(tokens, "input");
    let output = count(tokens, "output");
    let cache_read = count(cache, "read");
    let cache_creation = count(cache, "write");
    // No modelled token field → no event (a `{total}`-only step yields nothing
    // rather than an all-`None` Usage).
    input.or(output).or(cache_read).or(cache_creation)?;
    Some(Usage {
        message_id: part
            .get("messageID")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
        cost_usd: part.get("cost").and_then(Value::as_f64),
        source: UsageSource::Native,
    })
}

fn map_tool_status(s: &str) -> ToolStatus {
    match s {
        "completed" => ToolStatus::Completed,
        "error" => ToolStatus::Error,
        // pending / running / anything mid-flight
        _ => ToolStatus::Running,
    }
}

fn attention(reason: AttentionReason) -> Payload {
    Payload::AttentionRequired(AttentionRequired {
        reason,
        message: String::new(),
    })
}

/// Pull a message out of a `session.error` event's `error` (string, or an object
/// with `message` / `data.message`).
fn error_message(props: &Value) -> String {
    match props.get("error") {
        Some(Value::String(s)) => s.clone(),
        Some(obj @ Value::Object(_)) => obj
            .get("message")
            .or_else(|| obj.get("data").and_then(|d| d.get("message")))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Drain an opencode `/event` SSE stream into the durable [`SessionLog`],
/// mapping each event through [`EventMapper`]. The transport-agnostic core: the
/// caller hands a reader (a live HTTP body, or a `Cursor` in tests) and a stop
/// flag; we parse SSE frames (`data:` lines terminated by a blank line), map
/// each JSON envelope, and append the resulting §0 events.
///
/// Blocks reading the stream until it closes or `stop` is set (observed between
/// frames — a live caller closes the connection to unblock). Non-JSON `data:`
/// payloads and unmapped event types are skipped, not errored, so a stray frame
/// can't wedge the stream. Returns the number of §0 events appended.
pub(crate) fn drain_sse<R: std::io::Read>(
    reader: R,
    session_id: &str,
    log: &mut SessionLog,
    stop: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<usize> {
    use std::io::BufRead as _;
    use std::sync::atomic::Ordering;

    let mut mapper = EventMapper::new();
    let mut data = String::new();
    let mut total = 0;
    let mut lines = std::io::BufReader::new(reader).lines();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(line) = lines.next() else { break };
        let line = line?;
        // `lines()` strips `\n` but keeps a `\r` — tolerate CRLF SSE so a blank
        // `\r` line still terminates a frame and a `data:…\r` doesn't carry the
        // `\r` into the JSON. (opencode emits bare `\n` today; this is a guard.)
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            // SSE allows one optional space after the colon; multiple `data:`
            // lines in a frame join with newlines.
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() {
            // Blank line ends the frame.
            total += flush_frame(&mut mapper, &mut data, session_id, log)?;
        }
        // `event:` / `id:` / `retry:` / `:comment` lines carry no payload here.
    }
    // A stream that closed mid-frame (no trailing blank line) still flushes.
    total += flush_frame(&mut mapper, &mut data, session_id, log)?;
    Ok(total)
}

/// Map one accumulated SSE `data` payload (cleared afterward) into §0 events and
/// append them. A non-JSON or unmapped frame appends nothing.
fn flush_frame(
    mapper: &mut EventMapper,
    data: &mut String,
    session_id: &str,
    log: &mut SessionLog,
) -> anyhow::Result<usize> {
    if data.is_empty() {
        return Ok(0);
    }
    let parsed: Result<Value, _> = serde_json::from_str(data);
    data.clear();
    let Ok(value) = parsed else { return Ok(0) };
    // opencode's `/event` stream is the agent's own output — stamp it `agent`
    // (the host knows it launched opencode; the guest can't claim a different actor).
    let events: Vec<Event> = mapper
        .on_event(&value)
        .into_iter()
        .map(|p| Event::session(session_id, p).with_actor(Actor::agent("opencode")))
        .collect();
    if events.is_empty() {
        return Ok(0);
    }
    let n = events.len();
    log.append(&events)?;
    Ok(n)
}

/// A [`Read`](std::io::Read) over a growing file that **blocks at EOF** (polling)
/// instead of ending — so `drain_sse` follows the in-sandbox `/event` capture
/// file like `tail -F` (replay everything already there, then stream appends).
/// Reading a file being appended is safe: at EOF the offset holds, and a later
/// read returns bytes written past it. (Consumed by the libkrun file path;
/// docker §0 still uses the live bridge.)
///
/// Two subtleties the obvious version gets wrong:
/// - **Opens lazily by path.** The guest creates the file only when opencode
///   emits its first SSE line, so a `watch` right after `run` can beat it; we
///   poll for the file to appear rather than giving up (which would silently
///   capture nothing for the run-then-watch ordering).
/// - **Reads before checking `stop`.** On stop we do a final read first, so any
///   frames the guest flushed during the last poll sleep are still drained
///   (mirrors the file tailer's final-pump); only a genuine EOF *and* `stop`
///   ends the drain.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) struct FollowReader {
    path: std::path::PathBuf,
    file: Option<std::fs::File>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FollowReader {
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn new(
        path: std::path::PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            path,
            file: None,
            stop,
        }
    }
}

impl std::io::Read for FollowReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::atomic::Ordering;
        let nap = std::time::Duration::from_millis(200);
        loop {
            // Lazy open: wait for the guest to create the file (first SSE line).
            if self.file.is_none() {
                if self.stop.load(Ordering::Relaxed) {
                    return Ok(0);
                }
                match std::fs::File::open(&self.path) {
                    Ok(f) => self.file = Some(f),
                    Err(_) => {
                        std::thread::sleep(nap);
                        continue;
                    }
                }
            }
            // Read FIRST, then decide on `stop` — so a final read after stop is
            // observed still drains frames flushed during the previous nap.
            let n = self.file.as_mut().expect("opened above").read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            if self.stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            std::thread::sleep(nap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(ty: &str, props: Value) -> Value {
        json!({ "id": "evt_x", "type": ty, "properties": props })
    }

    // Envelope shapes below are trimmed from a real GLM turn captured through
    // `opencode serve` (opencode 1.15.10), so the mapper is tested against the
    // wire format, not a guess.

    #[test]
    fn assistant_message_then_text_deltas_then_idle() {
        let mut m = EventMapper::new();

        // The user-message echo of our own prompt maps to nothing.
        assert!(m
            .on_event(&ev(
                "message.updated",
                json!({ "sessionID": "ses_a", "info": { "id": "msg_u", "role": "user" } }),
            ))
            .is_empty());

        // First sight of the assistant message → MessageStart.
        let start = m.on_event(&ev(
            "message.updated",
            json!({ "sessionID": "ses_a", "info": {
                "id": "msg_a", "role": "assistant",
                "model": { "providerID": "zai-coding-plan", "modelID": "glm-4.5-air" } } }),
        ));
        assert!(matches!(&start[..],
            [Payload::MessageStart(s)] if s.message_id == "msg_a" && s.role == Role::Assistant));
        // A repeat update of the same message does NOT re-open it.
        assert!(m
            .on_event(&ev(
                "message.updated",
                json!({ "sessionID": "ses_a", "info": { "id": "msg_a", "role": "assistant" } }),
            ))
            .is_empty());

        // Streaming text deltas carry the messageID.
        let d = m.on_event(&ev(
            "message.part.delta",
            json!({ "sessionID": "ses_a", "messageID": "msg_a", "partID": "prt_1",
                    "field": "text", "delta": "hi" }),
        ));
        assert!(matches!(&d[..],
            [Payload::MessageDelta(x)] if x.text == "hi" && x.message_id == "msg_a"));
        // Empty deltas drop.
        assert!(m
            .on_event(&ev(
                "message.part.delta",
                json!({ "sessionID": "ses_a", "messageID": "msg_a", "field": "text", "delta": "" }),
            ))
            .is_empty());

        // Idle ends the open message and raises NeedsInput.
        let idle = m.on_event(&ev("session.idle", json!({ "sessionID": "ses_a" })));
        assert!(matches!(&idle[..],
            [Payload::MessageEnd(e), Payload::AttentionRequired(a)]
            if e.message_id == "msg_a" && a.reason == AttentionReason::NeedsInput));
    }

    #[test]
    fn schema_bound_output_maps_once_into_message_evidence() {
        let mut m = EventMapper::new();
        let updated = ev(
            "message.updated",
            json!({ "sessionID": "ses_a", "info": {
                "id": "msg_a",
                "role": "assistant",
                "structured": {
                    "kind": "document",
                    "text": "# Grill\n\nChallenge the assumptions."
                }
            } }),
        );
        let output = m.on_event(&updated);
        assert!(matches!(&output[..],
            [Payload::MessageStart(s), Payload::MessageDelta(d)]
            if s.message_id == "msg_a"
                && d.message_id == "msg_a"
                && d.text == "{\"kind\":\"document\",\"text\":\"# Grill\\n\\nChallenge the assumptions.\"}"));
        assert!(m.on_event(&updated).is_empty());
    }

    #[test]
    fn reasoning_delta_maps_to_thinking() {
        let mut m = EventMapper::new();
        let t = m.on_event(&ev(
            "message.part.delta",
            json!({ "sessionID": "s", "messageID": "m", "field": "reasoning", "delta": "hmm" }),
        ));
        assert!(matches!(&t[..], [Payload::Thinking(x)] if x.text == "hmm"));
    }

    #[test]
    fn tool_part_emits_on_status_change_only() {
        let mut m = EventMapper::new();
        let tool = |status: &str, input: Value| {
            ev(
                "message.part.updated",
                json!({ "sessionID": "s", "part": {
                    "id": "prt_t", "messageID": "m", "type": "tool",
                    "tool": "ls", "callID": "call_1",
                    "state": { "status": status, "input": input } } }),
            )
        };
        // pending → Running (with name + input).
        let a = m.on_event(&tool("pending", json!({ "path": "." })));
        assert!(matches!(&a[..], [Payload::ToolCall(t)]
            if t.name == "ls" && t.tool_call_id == "call_1" && t.status == ToolStatus::Running
               && t.input.as_ref().and_then(|i| i.get("path")).and_then(|x| x.as_str()) == Some(".")));
        // running → still Running, but status unchanged from our mapping → no dup.
        assert!(m
            .on_event(&tool("running", json!({ "path": "." })))
            .is_empty());
        // completed → Completed with output.
        let done = ev(
            "message.part.updated",
            json!({ "sessionID": "s", "part": {
                "type": "tool", "tool": "ls", "callID": "call_1",
                "state": { "status": "completed", "output": "a\nb" } } }),
        );
        assert!(matches!(&m.on_event(&done)[..],
            [Payload::ToolCall(t)] if t.status == ToolStatus::Completed && t.output == "a\nb"));
    }

    #[test]
    fn step_finish_emits_usage_native_once() {
        let mut m = EventMapper::new();
        let step = ev(
            "message.part.updated",
            json!({ "sessionID": "s", "part": {
                "id": "prt_step", "messageID": "msg_a", "type": "step-finish",
                "tokens": { "input": 120, "output": 30, "reasoning": 5,
                            "cache": { "read": 100, "write": 20 } } } }),
        );
        let out = m.on_event(&step);
        let [Payload::Usage(u)] = &out[..] else {
            panic!("expected one Usage: {out:?}");
        };
        assert_eq!(u.message_id, "msg_a");
        assert_eq!(u.input_tokens, Some(120));
        assert_eq!(u.output_tokens, Some(30));
        assert_eq!(u.cache_read_input_tokens, Some(100));
        assert_eq!(u.cache_creation_input_tokens, Some(20));
        assert_eq!(u.source, UsageSource::Native);
        // A re-sent part.updated for the same step id must not double-count.
        assert!(m.on_event(&step).is_empty());
    }

    #[test]
    fn step_finish_without_modelled_tokens_is_ignored() {
        let mut m = EventMapper::new();
        // A `{total}`-only step (the trimmed fixture shape) carries nothing we
        // model → no Usage rather than an all-`None` event.
        let step = ev(
            "message.part.updated",
            json!({ "part": { "id": "prt_s", "type": "step-finish",
                              "tokens": { "total": 10 } } }),
        );
        assert!(m.on_event(&step).is_empty());
    }

    #[test]
    fn snapshots_and_lifecycle_and_session_next_are_ignored() {
        let mut m = EventMapper::new();
        // text/reasoning part *snapshots* (deltas already carry their content),
        // step boundaries, session lifecycle, the session.next.* family, server.*
        for e in [
            ev(
                "message.part.updated",
                json!({ "part": { "type": "text", "text": "full text so far" } }),
            ),
            ev(
                "message.part.updated",
                json!({ "part": { "type": "step-finish", "tokens": { "total": 10 } } }),
            ),
            ev(
                "session.next.text.delta",
                json!({ "sessionID": "s", "delta": "x" }),
            ),
            ev("session.next.model.switched", json!({ "sessionID": "s" })),
            ev("session.updated", json!({ "sessionID": "s" })),
            ev("server.heartbeat", json!({})),
        ] {
            assert!(m.on_event(&e).is_empty(), "should ignore: {}", e["type"]);
        }
    }

    /// End-to-end transport: a raw `/event` SSE byte stream (a text turn that
    /// goes idle) drains into the durable log as the mapped §0 events — the same
    /// sink `session watch`/`subscribe` read, so opencode lights up there with
    /// no transcript file.
    #[test]
    fn drain_sse_feeds_the_durable_log() {
        use std::io::Cursor;
        use std::sync::atomic::AtomicBool;

        crate::test_util::with_isolated_home("opencode-drain-sse", || {
            let pb = crate::pillbox::global();
            let mut log = SessionLog::open(&pb, "ses-oc").expect("open log");

            let stream = "\
data: {\"type\":\"server.connected\",\"properties\":{}}\n\
\n\
data: {\"type\":\"message.updated\",\"properties\":{\"info\":{\"id\":\"msg_a\",\"role\":\"assistant\"}}}\n\
\n\
data: {\"type\":\"message.part.delta\",\"properties\":{\"messageID\":\"msg_a\",\"field\":\"text\",\"delta\":\"hi \"}}\n\
\n\
data: {\"type\":\"message.part.delta\",\"properties\":{\"messageID\":\"msg_a\",\"field\":\"text\",\"delta\":\"there\"}}\n\
\n\
data: {\"type\":\"message.part.updated\",\"properties\":{\"part\":{\"id\":\"prt_s\",\"messageID\":\"msg_a\",\"type\":\"step-finish\",\"tokens\":{\"input\":12,\"output\":4,\"cache\":{\"read\":8,\"write\":0}}}}}\n\
\n\
data: {\"type\":\"session.idle\",\"properties\":{\"sessionID\":\"ses_oc\"}}\n\
\n";

            let stop = AtomicBool::new(false);
            let n = drain_sse(Cursor::new(stream), "ses-oc", &mut log, &stop).expect("drain");
            assert_eq!(n, 6, "start + 2 deltas + usage + end + attention");

            let events = SessionLog::open(&pb, "ses-oc")
                .unwrap()
                .read_from(0)
                .unwrap();
            use crate::contract::Payload as P;
            assert!(matches!(events[0].payload, P::MessageStart(_)));
            assert!(matches!(&events[1].payload, P::MessageDelta(d) if d.text == "hi "));
            assert!(matches!(&events[2].payload, P::MessageDelta(d) if d.text == "there"));
            assert!(matches!(&events[3].payload,
                P::Usage(u) if u.input_tokens == Some(12) && u.output_tokens == Some(4)
                    && u.cache_read_input_tokens == Some(8) && u.source == UsageSource::Native));
            assert!(matches!(events[4].payload, P::MessageEnd(_)));
            assert!(matches!(&events[5].payload,
                P::AttentionRequired(a) if a.reason == AttentionReason::NeedsInput));
            assert_eq!(
                events.iter().map(|e| e.seq).collect::<Vec<_>>(),
                vec![1, 2, 3, 4, 5, 6]
            );
            // Every drained event is stamped as the opencode agent.
            assert!(events
                .iter()
                .all(|e| e.actor == Some(crate::contract::Actor::agent("opencode"))));
        });
    }
}
