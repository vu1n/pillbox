//! Workspace transcript ingest — read agent-native JSONL transcripts
//! and emit OTLP child spans of the session span.
//!
//! Pillbox owns the workspace, which means it has access to every
//! transcript an agent harness writes inside the sandbox (Claude
//! Code's `~/.claude/projects/<encoded>/<uuid>.jsonl`, Codex's
//! `~/.codex/sessions/...`). These files capture harness-internal
//! state the wire never sees — tool inputs/outputs, user prompts,
//! the final assembled assistant content — and they exist for every
//! harness, including ones without hooks (Codex). This is the third
//! telemetry source on top of the MITM (universal floor) and hooks
//! (per-harness quality), and the largest.
//!
//! First cut is **drain-mode** only: a CLI driver
//! (`pillbox session transcript`) reads a completed transcript file
//! and emits spans. Live-tailing via FS-watcher + bind-mount of the
//! sandbox transcript dir is the next layer; once shipped this
//! same parser + emitter feed it without rework.
//!
//! Spans nest under the session span via the existing trace
//! correlation: each transcript span shares the session's
//! `trace_id` and its `parent_span_id` is the session span's
//! deterministic span_id. Workshop / any OTLP consumer renders the
//! result as one trace per pillbox run with user prompts, assistant
//! messages, tool invocations as named children.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanContext, TraceContextExt as _, TraceFlags, TraceState, Tracer,
};
use opentelemetry::Context as OtelContext;
use opentelemetry::KeyValue;

use super::otel::spans::{derive_session_span_id, derive_trace_id, tracer};

mod claude;

/// One parsed transcript line, in a harness-agnostic shape so a
/// future Codex parser can produce the same kind of events.
#[derive(Debug, Clone)]
pub(crate) struct TranscriptEvent {
    /// Stable id for this event — used as the span_id (via
    /// deterministic packing) so re-emitting the same transcript
    /// produces the same spans rather than duplicates. Claude
    /// Code's per-line `uuid` is the natural fit; lines that fan
    /// out (assistant messages with multiple content blocks)
    /// suffix the block index.
    pub uuid: String,
    /// Chain pointer to the previous line. Currently surfaced as
    /// an attribute only — span parenting goes through the session
    /// span — but kept on the event so a future
    /// "exact-chain" view can rebuild the linked-list shape.
    pub parent_uuid: Option<String>,
    pub timestamp: SystemTime,
    pub kind: EventKind,
}

#[derive(Debug, Clone)]
pub(crate) enum EventKind {
    UserPrompt {
        content: String,
    },
    AssistantText {
        text: String,
        model: Option<String>,
        usage: Option<AssistantUsage>,
        stop_reason: Option<String>,
    },
    AssistantThinking {
        text: String,
    },
    ToolUse {
        tool_use_id: String,
        tool_name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// Per-message token usage from `message_start` / `message_delta`,
/// mirrored on the assistant-message span so consumers don't have
/// to join transcript spans with vault MITM spans to get per-message
/// cost.
#[derive(Debug, Clone, Default)]
pub(crate) struct AssistantUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
}

/// Drain one transcript file and emit one OTLP child span per
/// rendered event. Returns the number of spans emitted (zero when
/// the OTel tracer isn't configured — the parser still runs,
/// produces no observable side effects).
pub(crate) fn drain_file(path: &Path, session_id: &str) -> Result<usize> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut count = 0;
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        for event in claude::parse_line(line) {
            emit_event_span(&event, session_id);
            count += 1;
        }
    }
    Ok(count)
}

/// Build + flush one span for the event under the session span.
/// Skips silently when no OTel endpoint is configured (the parser
/// still ran — useful for `drain_file`'s count when the user wants
/// to verify the parse without standing up a collector).
fn emit_event_span(event: &TranscriptEvent, session_id: &str) {
    let Some(tracer) = tracer() else {
        return;
    };
    let trace_id = derive_trace_id(session_id);
    let parent_ctx = OtelContext::new().with_remote_span_context(SpanContext::new(
        trace_id,
        derive_session_span_id(session_id),
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    ));
    let builder = SpanBuilder::from_name(span_name(&event.kind))
        .with_trace_id(trace_id)
        .with_start_time(event.timestamp)
        .with_end_time(event.timestamp)
        .with_attributes(build_attrs(event, session_id));
    let mut span = tracer.build_with_context(builder, &parent_ctx);
    span.end();
}

fn span_name(kind: &EventKind) -> String {
    match kind {
        EventKind::UserPrompt { .. } => "user.prompt".into(),
        EventKind::AssistantText { .. } => "assistant.text".into(),
        EventKind::AssistantThinking { .. } => "assistant.thinking".into(),
        EventKind::ToolUse { tool_name, .. } => format!("tool {tool_name}"),
        EventKind::ToolResult { .. } => "tool.result".into(),
    }
}

/// Maximum bytes we'll attach for prompt/completion/tool I/O. The
/// OTel spec leaves attribute size up to the SDK; 4 KB is enough
/// for human-readable inspection without ballooning the protobuf
/// payload on long-context assistant turns.
const MAX_BODY_BYTES: usize = 4096;

fn build_attrs(event: &TranscriptEvent, session_id: &str) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new("gen_ai.system", "anthropic"),
        KeyValue::new("pillbox.session_id", session_id.to_string()),
        KeyValue::new("pillbox.transcript.uuid", event.uuid.clone()),
    ];
    if let Some(p) = event.parent_uuid.as_deref() {
        attrs.push(KeyValue::new(
            "pillbox.transcript.parent_uuid",
            p.to_string(),
        ));
    }
    match &event.kind {
        EventKind::UserPrompt { content } => {
            attrs.push(KeyValue::new("gen_ai.prompt", truncate(content)));
        }
        EventKind::AssistantText {
            text,
            model,
            usage,
            stop_reason,
        } => {
            attrs.push(KeyValue::new("gen_ai.completion", truncate(text)));
            if let Some(m) = model.as_deref() {
                attrs.push(KeyValue::new("gen_ai.response.model", m.to_string()));
            }
            if let Some(s) = stop_reason.as_deref() {
                attrs.push(KeyValue::new(
                    "gen_ai.response.finish_reasons",
                    s.to_string(),
                ));
            }
            if let Some(u) = usage {
                push_usage_attrs(&mut attrs, u);
            }
        }
        EventKind::AssistantThinking { text } => {
            attrs.push(KeyValue::new("gen_ai.completion.thinking", truncate(text)));
        }
        EventKind::ToolUse {
            tool_use_id,
            tool_name,
            input,
        } => {
            attrs.push(KeyValue::new("gen_ai.tool.name", tool_name.to_string()));
            attrs.push(KeyValue::new(
                "gen_ai.tool.call.id",
                tool_use_id.to_string(),
            ));
            let serialized = serde_json::to_string(input).unwrap_or_default();
            attrs.push(KeyValue::new(
                "gen_ai.tool.arguments",
                truncate(&serialized),
            ));
        }
        EventKind::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            attrs.push(KeyValue::new(
                "gen_ai.tool.call.id",
                tool_use_id.to_string(),
            ));
            attrs.push(KeyValue::new("gen_ai.tool.result", truncate(content)));
            attrs.push(KeyValue::new("gen_ai.tool.is_error", *is_error));
        }
    }
    attrs
}

/// Mirror the gen_ai usage attrs the vault MITM emits, so an
/// assistant-message span shows the same numbers without joining
/// against the wire-level span by request id. Attr names are
/// duplicated rather than imported because the vault path's
/// usage shape lives behind `pub(crate)` re-exports — keep this
/// in sync with `events::otel::genai::push_usage_attrs`.
fn push_usage_attrs(attrs: &mut Vec<KeyValue>, u: &AssistantUsage) {
    if let Some(n) = u.input_tokens {
        attrs.push(KeyValue::new("gen_ai.usage.input_tokens", n as i64));
    }
    if let Some(n) = u.output_tokens {
        attrs.push(KeyValue::new("gen_ai.usage.output_tokens", n as i64));
    }
    if let Some(n) = u.cache_read_input_tokens {
        attrs.push(KeyValue::new(
            "gen_ai.usage.cache_read_input_tokens",
            n as i64,
        ));
    }
    if let Some(n) = u.cache_creation_input_tokens {
        attrs.push(KeyValue::new(
            "gen_ai.usage.cache_creation_input_tokens",
            n as i64,
        ));
    }
}

/// Cap large strings at [`MAX_BODY_BYTES`]; append a single `…` so
/// truncation is visually obvious in collector UIs.
fn truncate(s: &str) -> String {
    if s.len() <= MAX_BODY_BYTES {
        return s.to_string();
    }
    let mut end = MAX_BODY_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_names_use_dotted_namespace() {
        // Pinning the wire-name shape so a refactor that breaks the
        // namespace surfaces here rather than as silently-orphaned
        // dashboard queries.
        assert_eq!(
            span_name(&EventKind::UserPrompt {
                content: "x".into()
            }),
            "user.prompt",
        );
        assert_eq!(
            span_name(&EventKind::AssistantText {
                text: "x".into(),
                model: None,
                usage: None,
                stop_reason: None,
            }),
            "assistant.text",
        );
        assert_eq!(
            span_name(&EventKind::AssistantThinking { text: "x".into() }),
            "assistant.thinking",
        );
        assert_eq!(
            span_name(&EventKind::ToolUse {
                tool_use_id: "tu".into(),
                tool_name: "Bash".into(),
                input: serde_json::Value::Null,
            }),
            "tool Bash",
        );
        assert_eq!(
            span_name(&EventKind::ToolResult {
                tool_use_id: "tu".into(),
                content: "x".into(),
                is_error: false,
            }),
            "tool.result",
        );
    }

    #[test]
    fn build_attrs_carries_all_user_prompt_fields() {
        let event = TranscriptEvent {
            uuid: "u1".into(),
            parent_uuid: Some("p1".into()),
            timestamp: SystemTime::now(),
            kind: EventKind::UserPrompt {
                content: "hello".into(),
            },
        };
        let attrs = build_attrs(&event, "sess-xyz");
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        for expected in [
            "gen_ai.system",
            "gen_ai.prompt",
            "pillbox.session_id",
            "pillbox.transcript.uuid",
            "pillbox.transcript.parent_uuid",
        ] {
            assert!(keys.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn truncate_caps_long_input_with_ellipsis() {
        let long = "a".repeat(MAX_BODY_BYTES + 100);
        let out = truncate(&long);
        assert!(out.ends_with('…'));
        // ASCII so byte len == char len up to the marker (3 bytes).
        assert!(out.len() <= MAX_BODY_BYTES + 4);
    }

    #[test]
    fn truncate_short_input_passes_through() {
        assert_eq!(truncate("short"), "short");
    }

    #[test]
    fn truncate_handles_multibyte_char_boundary() {
        // Build a string where the cut would land mid-codepoint.
        let mut s = "a".repeat(MAX_BODY_BYTES - 1);
        s.push('🌶'); // 4 bytes; pushes total past MAX_BODY_BYTES
        let out = truncate(&s);
        assert!(out.ends_with('…'));
        // Doesn't panic; truncation walked back to a char boundary.
    }

    #[test]
    fn emit_is_noop_without_endpoint_configured() {
        // tracer() returns None when OTEL_EXPORTER_OTLP_ENDPOINT is
        // unset. drain_file should still parse + count even without
        // emission. (As with the genai emit caveat, this exercises
        // either path depending on test ordering in the binary; the
        // observable result — counted parses, no panic — holds.)
        let event = TranscriptEvent {
            uuid: "u1".into(),
            parent_uuid: None,
            timestamp: SystemTime::now(),
            kind: EventKind::UserPrompt {
                content: "x".into(),
            },
        };
        emit_event_span(&event, "sess-xyz"); // no-op or live; either is OK
    }
}
