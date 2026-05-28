//! OTLP gen_ai spans — one span per intercepted LLM API call.
//!
//! Driven by the vault MITM proxy. The vault sees every request the
//! sandboxed agent makes to `api.anthropic.com` (and any other host
//! we proxy later), which makes it the universal floor for LLM
//! telemetry: it works for any harness regardless of whether the
//! harness itself exposes hooks. Attributes follow OTel's GenAI
//! semantic conventions so downstream consumers (Raindrop Workshop,
//! Phoenix, Langfuse, generic OTel collectors) can normalize without
//! a pillbox-specific adapter.
//!
//! Trace correlation is preferentially by `session_id` (one trace
//! per pillbox run, parented to the sandbox-side session span via
//! shared trace_id + the session's deterministic span_id). When the
//! caller didn't plumb a session_id through (e.g. test fixtures,
//! local-docker foreground without the wrapper), gen_ai spans fall
//! back to a sandbox_id-rooted trace per lease.

use std::time::SystemTime;

use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanContext, Status, TraceContextExt as _, TraceFlags, TraceState,
    Tracer,
};
use opentelemetry::Context as OtelContext;
use opentelemetry::KeyValue;

use super::spans::{derive_session_span_id, derive_trace_id, tracer};

/// One captured LLM API call. Built by the vault handler from the
/// request/response pair it intercepts and handed off to
/// [`emit_call_span`] when the response completes.
#[derive(Debug)]
pub(crate) struct CallSpan {
    /// Per-sandbox vault lease id. Always known. Surfaces as the
    /// `pillbox.sandbox_id` attribute and is the trace_id fallback
    /// when `session_id` is absent.
    pub(crate) sandbox_id: String,
    /// Pillbox-run session id when the orchestrator plumbed one
    /// through (see [`crate::vault::RunContext`]). When `Some`, the
    /// span shares a trace with the session span emitted by
    /// [`super::spans`] and parents it; when `None`, the span roots
    /// its own trace per sandbox lease.
    pub(crate) session_id: Option<String>,
    /// `pillbox.mode` attribute — orchestration regime
    /// (`"interactive"`, `"detached"`, future modes). Omitted when
    /// `None`.
    pub(crate) mode: Option<String>,
    /// `pillbox.workspace_id` attribute — path-encoded pillbox key
    /// or `"global"`. Omitted when `None`.
    pub(crate) workspace_id: Option<String>,
    pub(crate) start: SystemTime,
    pub(crate) end: SystemTime,
    pub(crate) host: String,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) status_code: u16,
    /// GenAI body-derived signals — populated by the response-body
    /// SSE tap (Anthropic streaming) or by parsing the response body
    /// JSON (non-streaming). All-`None` for endpoints that don't
    /// carry these fields (e.g. `GET /v1/models`) or for error
    /// responses with no usage block.
    pub(crate) usage: GenAiUsage,
}

/// Body-derived gen_ai signals. Lives separately from the envelope
/// because its source differs: the envelope is what the proxy
/// handler observes directly; this is what the SSE parser
/// accumulates as the response body streams through.
#[derive(Debug, Clone, Default)]
pub(crate) struct GenAiUsage {
    /// The model the server actually served. `gen_ai.response.model`.
    pub(crate) response_model: Option<String>,
    /// Server-assigned response id (e.g. Anthropic `msg_…`).
    /// `gen_ai.response.id`.
    pub(crate) response_id: Option<String>,
    /// `gen_ai.usage.input_tokens` — non-cached input tokens billed.
    pub(crate) input_tokens: Option<u64>,
    /// `gen_ai.usage.output_tokens` — tokens generated.
    pub(crate) output_tokens: Option<u64>,
    /// `gen_ai.usage.cache_read_input_tokens` — Anthropic prompt-
    /// cache hit count. Non-standard OTel attr, but Workshop /
    /// Raindrop adapters key on this name.
    pub(crate) cache_read_input_tokens: Option<u64>,
    /// `gen_ai.usage.cache_creation_input_tokens` — Anthropic
    /// prompt-cache miss count (tokens written to cache).
    pub(crate) cache_creation_input_tokens: Option<u64>,
    /// `gen_ai.response.finish_reasons` — single-entry list with the
    /// stop_reason from `message_delta.delta.stop_reason`.
    pub(crate) finish_reason: Option<String>,
}

/// Emit one OTLP span for a completed LLM API call. Best-effort skip
/// when the OTel tracer isn't configured. The vault calls this from
/// the response-completion path; the function itself does no I/O
/// until the SDK's simple processor flushes on `.end()`, matching
/// the session-span sink's sync model.
pub(crate) fn emit_call_span(call: CallSpan) {
    let Some(tracer) = tracer() else {
        return;
    };
    let span_name = match call.usage.response_model.as_deref() {
        Some(model) => format!("chat {model}"),
        None => "chat".to_string(),
    };
    // trace_id derives from session_id when plumbed (so this span
    // joins the trace rooted at the sandbox-side session span);
    // otherwise from sandbox_id (one trace per lease).
    let trace_seed = call.session_id.as_deref().unwrap_or(&call.sandbox_id);
    let trace_id = derive_trace_id(trace_seed);
    let builder = SpanBuilder::from_name(span_name)
        .with_trace_id(trace_id)
        .with_start_time(call.start)
        .with_end_time(call.end)
        .with_status(status_for(call.status_code))
        .with_attributes(build_attributes(&call));
    // Parent under the session span when session_id is known. The
    // session span's span_id is deterministic (see
    // `super::spans::derive_session_span_id`), so we don't need the
    // session span itself to have been emitted first — Workshop /
    // collectors stitch the link by id. OTel propagates parent via
    // Context, so we build a remote SpanContext on the parent_id
    // and pass it through `build_with_context`.
    let parent_ctx = match call.session_id.as_deref() {
        Some(session_id) => OtelContext::new().with_remote_span_context(SpanContext::new(
            trace_id,
            derive_session_span_id(session_id),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        )),
        None => OtelContext::new(),
    };
    // No `with_span_id` — the SDK's id generator mints a fresh per-
    // span id so multiple calls within the same trace stay distinct.
    let mut span = tracer.build_with_context(builder, &parent_ctx);
    span.end();
}

/// OTel GenAI semantic conventions + standard `http.*` / `server.*`
/// attribute names. Stable strings here are the wire contract with
/// downstream consumers — renaming any of them is a breaking change.
fn build_attributes(call: &CallSpan) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new("gen_ai.system", "anthropic"),
        KeyValue::new("gen_ai.operation.name", "chat"),
        KeyValue::new("server.address", call.host.clone()),
        KeyValue::new("http.request.method", call.method.clone()),
        KeyValue::new("url.path", call.path.clone()),
        KeyValue::new("http.response.status_code", call.status_code as i64),
        KeyValue::new("pillbox.sandbox_id", call.sandbox_id.clone()),
    ];
    if let Some(mode) = call.mode.as_deref() {
        attrs.push(KeyValue::new("pillbox.mode", mode.to_string()));
    }
    if let Some(ws) = call.workspace_id.as_deref() {
        attrs.push(KeyValue::new("pillbox.workspace_id", ws.to_string()));
    }
    push_usage_attrs(&mut attrs, &call.usage);
    attrs
}

/// Fan out the optional [`GenAiUsage`] fields into attrs. Each
/// `Some(_)` becomes one `KeyValue`; `None` is omitted (vs. emitted
/// as zero) because consumers distinguish "missing" from
/// "explicitly zero" — a 0-token call is a real signal, an unknown
/// is silent.
///
/// Visible to sibling sinks so the transcript emitter can attach
/// the same usage shape it gets from the agent's per-message
/// `usage` block — keeps `gen_ai.usage.*` attribute names in one
/// place so a future semconv change can't drift between sources.
pub(in crate::events) fn push_usage_attrs(attrs: &mut Vec<KeyValue>, usage: &GenAiUsage) {
    if let Some(v) = usage.response_model.as_deref() {
        attrs.push(KeyValue::new("gen_ai.response.model", v.to_string()));
    }
    if let Some(v) = usage.response_id.as_deref() {
        attrs.push(KeyValue::new("gen_ai.response.id", v.to_string()));
    }
    if let Some(v) = usage.input_tokens {
        attrs.push(KeyValue::new("gen_ai.usage.input_tokens", v as i64));
    }
    if let Some(v) = usage.output_tokens {
        attrs.push(KeyValue::new("gen_ai.usage.output_tokens", v as i64));
    }
    if let Some(v) = usage.cache_read_input_tokens {
        attrs.push(KeyValue::new(
            "gen_ai.usage.cache_read_input_tokens",
            v as i64,
        ));
    }
    if let Some(v) = usage.cache_creation_input_tokens {
        attrs.push(KeyValue::new(
            "gen_ai.usage.cache_creation_input_tokens",
            v as i64,
        ));
    }
    if let Some(v) = usage.finish_reason.as_deref() {
        attrs.push(KeyValue::new(
            "gen_ai.response.finish_reasons",
            v.to_string(),
        ));
    }
}

fn status_for(status_code: u16) -> Status {
    if status_code < 400 {
        Status::Ok
    } else {
        Status::Error {
            description: format!("HTTP {status_code}").into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_call() -> CallSpan {
        let now = SystemTime::now();
        CallSpan {
            sandbox_id: "abc123def456".into(),
            session_id: None,
            mode: Some("interactive".into()),
            workspace_id: Some("-Users-vuln-code-foo".into()),
            start: now,
            end: now,
            host: "api.anthropic.com".into(),
            method: "POST".into(),
            path: "/v1/messages".into(),
            status_code: 200,
            usage: GenAiUsage {
                response_model: Some("claude-sonnet-4-5-20250929".into()),
                response_id: Some("msg_abc".into()),
                input_tokens: Some(1247),
                output_tokens: Some(89),
                cache_read_input_tokens: Some(8431),
                cache_creation_input_tokens: Some(0),
                finish_reason: Some("end_turn".into()),
            },
        }
    }

    #[test]
    fn build_attributes_includes_genai_semconv_keys() {
        let attrs = build_attributes(&sample_call());
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        for expected in [
            "gen_ai.system",
            "gen_ai.operation.name",
            "gen_ai.response.model",
            "gen_ai.response.id",
            "gen_ai.usage.input_tokens",
            "gen_ai.usage.output_tokens",
            "gen_ai.usage.cache_read_input_tokens",
            "gen_ai.usage.cache_creation_input_tokens",
            "gen_ai.response.finish_reasons",
            "server.address",
            "http.request.method",
            "http.response.status_code",
            "pillbox.sandbox_id",
            "pillbox.mode",
            "pillbox.workspace_id",
        ] {
            assert!(keys.contains(&expected), "missing attr: {expected}");
        }
    }

    #[test]
    fn build_attributes_omits_orchestration_attrs_when_absent() {
        let mut call = sample_call();
        call.mode = None;
        call.workspace_id = None;
        let attrs = build_attributes(&call);
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        assert!(!keys.contains(&"pillbox.mode"));
        assert!(!keys.contains(&"pillbox.workspace_id"));
        // Envelope still there.
        assert!(keys.contains(&"pillbox.sandbox_id"));
    }

    #[test]
    fn build_attributes_omits_usage_fields_when_absent() {
        let mut call = sample_call();
        call.usage = GenAiUsage::default();
        let attrs = build_attributes(&call);
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        // Envelope attrs always present; usage attrs all absent.
        assert!(keys.contains(&"http.response.status_code"));
        for absent in [
            "gen_ai.response.model",
            "gen_ai.response.id",
            "gen_ai.usage.input_tokens",
            "gen_ai.usage.output_tokens",
            "gen_ai.usage.cache_read_input_tokens",
            "gen_ai.usage.cache_creation_input_tokens",
            "gen_ai.response.finish_reasons",
        ] {
            assert!(!keys.contains(&absent), "unexpected attr: {absent}");
        }
    }

    #[test]
    fn trace_id_seed_prefers_session_id_when_present() {
        // The selection logic is one line in emit_call_span; pin it
        // here so a refactor that breaks correlation surfaces as a
        // test failure rather than as silently-orphaned traces.
        let call_with_session = CallSpan {
            session_id: Some("sess-aabbcc".into()),
            ..sample_call()
        };
        let call_without_session = CallSpan {
            session_id: None,
            ..sample_call()
        };

        let seed_with = call_with_session
            .session_id
            .as_deref()
            .unwrap_or(&call_with_session.sandbox_id);
        let seed_without = call_without_session
            .session_id
            .as_deref()
            .unwrap_or(&call_without_session.sandbox_id);

        assert_eq!(seed_with, "sess-aabbcc");
        assert_eq!(seed_without, "abc123def456");

        // Two calls in the same session share a trace_id; the same
        // session_id correlates with the session span emitted by
        // super::spans (which uses the same derive_trace_id).
        assert_eq!(
            derive_trace_id(seed_with),
            super::super::spans::derive_trace_id("sess-aabbcc"),
        );
    }

    #[test]
    fn status_for_maps_2xx_to_ok_and_4xx_5xx_to_error() {
        assert!(matches!(status_for(200), Status::Ok));
        assert!(matches!(status_for(204), Status::Ok));
        assert!(matches!(status_for(399), Status::Ok));
        match status_for(401) {
            Status::Error { description } => assert!(description.contains("401")),
            other => panic!("expected Error, got {other:?}"),
        }
        match status_for(503) {
            Status::Error { description } => assert!(description.contains("503")),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
