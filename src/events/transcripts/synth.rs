//! Whole-chat span synthesis from the parsed transcript.
//!
//! Raindrop Workshop's "Overview" (ChatFlow) renders the conversation
//! from *whole-chat* LLM spans — each carrying the full message history
//! plus that turn's output, the way a real LLM API call looks. Our
//! per-event transcript spans (`user.prompt` / `assistant.text` /
//! `tool …`) populate the Span Tree but not Overview (ChatFlow's
//! growing-history dedup needs whole-chat spans, not per-message ones).
//!
//! This synthesizer reconstructs the whole-chat spans from the same
//! transcript events — the *canonical* source Raindrop itself reads
//! (the rollout / projects jsonl) — and emits them as gen_ai `chat`
//! spans through the shared [`emit_genai_call_span`] path, with
//! `gen_ai.input.messages` / `gen_ai.output.messages` in OTel semconv.
//! It sidesteps MITM'ing each provider's live wire (Codex's WebSocket +
//! Responses API + compressed bodies), and works uniformly for any
//! harness with a transcript parser.
//!
//! Used when the live wire isn't MITM'd for conversation content —
//! Codex always, and Claude run without `--vault`. When the MITM
//! already emits gen_ai chat spans (Claude + `--vault`), synthesis is
//! disabled by the caller so ChatFlow doesn't see each turn twice.
//!
//! Turn model: a `user.prompt` opens a turn; `assistant.text` blocks
//! accumulate as the reply; the turn is emitted when the *next* user
//! prompt arrives (or at [`finish`](ChatSynthesizer::finish), i.e. drain
//! end / follow stop). Tool calls/results and thinking stay in the Span
//! Tree — the synthesized chat keeps the user/assistant dialogue clean.

use std::time::SystemTime;

use super::{EventKind, TranscriptEvent};
use crate::events::{emit_genai_call_span, GenAiCallSpan, GenAiUsage};

struct Msg {
    role: &'static str,
    content: String,
}

/// Accumulates transcript events into a running conversation and emits
/// one whole-chat gen_ai span per completed assistant turn.
pub(super) struct ChatSynthesizer {
    session_id: String,
    history: Vec<Msg>,
    pending: Option<Pending>,
}

struct Pending {
    text: String,
    model: Option<String>,
    usage: Option<GenAiUsage>,
    ts: SystemTime,
}

impl ChatSynthesizer {
    pub(super) fn new(session_id: String) -> Self {
        Self {
            session_id,
            history: Vec::new(),
            pending: None,
        }
    }

    pub(super) fn on_event(&mut self, event: &TranscriptEvent) {
        match &event.kind {
            EventKind::UserPrompt { content } => {
                // A new user turn closes any in-flight assistant reply.
                self.flush();
                self.history.push(Msg {
                    role: "user",
                    content: content.clone(),
                });
            }
            EventKind::AssistantText {
                text, model, usage, ..
            } => match &mut self.pending {
                // Multiple content blocks in one assistant message arrive
                // as separate events — concatenate them into one reply.
                Some(p) => {
                    if !p.text.is_empty() && !text.is_empty() {
                        p.text.push('\n');
                    }
                    p.text.push_str(text);
                    if usage.is_some() {
                        p.usage = usage.clone();
                    }
                    if model.is_some() {
                        p.model = model.clone();
                    }
                    p.ts = event.timestamp;
                }
                None => {
                    self.pending = Some(Pending {
                        text: text.clone(),
                        model: model.clone(),
                        usage: usage.clone(),
                        ts: event.timestamp,
                    });
                }
            },
            // tool_use / tool_result / thinking stay in the Span Tree.
            _ => {}
        }
    }

    /// Flush the final turn at end-of-transcript (drain end / follow stop).
    pub(super) fn finish(&mut self) {
        self.flush();
    }

    /// Emit the pending assistant turn as a whole-chat span (history so
    /// far = input, this reply = output), then fold the reply into
    /// history so the next turn's input includes it.
    fn flush(&mut self) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let input_messages = messages_json(&self.history);
        let mut usage = p.usage.unwrap_or_default();
        usage.output_messages = Some(assistant_output_json(&p.text));
        if usage.response_model.is_none() {
            usage.response_model = p.model.clone();
        }
        emit_genai_call_span(GenAiCallSpan {
            sandbox_id: self.session_id.clone(),
            session_id: Some(self.session_id.clone()),
            mode: None,
            workspace_id: None,
            start: p.ts,
            end: p.ts,
            // Synthetic envelope: this span is reconstructed from the
            // transcript, not observed on the wire.
            host: "transcript".into(),
            method: String::new(),
            path: String::new(),
            status_code: 200,
            usage,
            input_messages: Some(input_messages),
            system_instructions: None,
        });
        self.history.push(Msg {
            role: "assistant",
            content: p.text,
        });
    }
}

/// `[{role, content}, …]` for `gen_ai.input.messages`.
fn messages_json(history: &[Msg]) -> String {
    let arr: Vec<serde_json::Value> = history
        .iter()
        .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
        .collect();
    serde_json::Value::Array(arr).to_string()
}

/// `[{role:"assistant", content}]` for `gen_ai.output.messages`.
fn assistant_output_json(text: &str) -> String {
    serde_json::json!([{ "role": "assistant", "content": text }]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(c: &str) -> TranscriptEvent {
        TranscriptEvent {
            uuid: "u".into(),
            parent_uuid: None,
            timestamp: SystemTime::now(),
            kind: EventKind::UserPrompt { content: c.into() },
        }
    }
    fn assistant(t: &str) -> TranscriptEvent {
        TranscriptEvent {
            uuid: "a".into(),
            parent_uuid: None,
            timestamp: SystemTime::now(),
            kind: EventKind::AssistantText {
                text: t.into(),
                model: Some("gpt-5.5".into()),
                usage: None,
                stop_reason: None,
            },
        }
    }

    #[test]
    fn history_grows_into_whole_chat_input() {
        let mut s = ChatSynthesizer::new("sess".into());
        s.on_event(&user("hi"));
        s.on_event(&assistant("hello"));
        // Reply not folded into history until the turn flushes.
        assert_eq!(
            messages_json(&s.history),
            r#"[{"content":"hi","role":"user"}]"#
        );
        s.on_event(&user("again")); // flushes turn 1 → folds assistant("hello")
                                    // Now history = [user:hi, assistant:hello, user:again].
        let parsed: serde_json::Value = serde_json::from_str(&messages_json(&s.history)).unwrap();
        let roles: Vec<&str> = parsed
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
    }

    #[test]
    fn assistant_blocks_concatenate_into_one_reply() {
        let mut s = ChatSynthesizer::new("sess".into());
        s.on_event(&user("q"));
        s.on_event(&assistant("part one"));
        s.on_event(&assistant("part two"));
        s.finish(); // flush the only turn
                    // The reply folded into history is the concatenation.
        let last = s.history.last().unwrap();
        assert_eq!(last.role, "assistant");
        assert_eq!(last.content, "part one\npart two");
    }

    #[test]
    fn output_json_escapes_content() {
        assert_eq!(
            assistant_output_json("a\"b"),
            r#"[{"content":"a\"b","role":"assistant"}]"#
        );
    }
}
