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
//! Trace correlation is by sandbox lease — all calls within one
//! `pillbox run --vault` share a `trace_id` derived from the sandbox
//! id. Session-level parenting (linking gen_ai spans to the session
//! span emitted by [`super::spans`]) is deferred until the orchestrator
//! plumbs `session_id` into the vault.

use std::time::SystemTime;

use opentelemetry::trace::{Span as _, SpanBuilder, Status, Tracer};
use opentelemetry::Context as OtelContext;
use opentelemetry::KeyValue;

use super::spans::{derive_trace_id, tracer};

/// One captured LLM API call. Built by the vault handler from the
/// request/response pair it intercepts and handed off to
/// [`emit_call_span`] when the response completes.
#[derive(Debug)]
pub(crate) struct CallSpan {
    pub(crate) sandbox_id: String,
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
#[derive(Debug, Default)]
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
    // No `with_span_id` — the SDK's id generator mints a fresh per-span
    // id so multiple calls within the same sandbox lease keep distinct
    // spans under the shared trace.
    let builder = SpanBuilder::from_name(span_name)
        .with_trace_id(derive_trace_id(&call.sandbox_id))
        .with_start_time(call.start)
        .with_end_time(call.end)
        .with_status(status_for(call.status_code))
        .with_attributes(build_attributes(&call));
    let mut span = tracer.build_with_context(builder, &OtelContext::new());
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
    push_usage_attrs(&mut attrs, &call.usage);
    attrs
}

/// Fan out the optional [`GenAiUsage`] fields into attrs. Each
/// `Some(_)` becomes one `KeyValue`; `None` is omitted (vs. emitted
/// as zero) because consumers distinguish "missing" from
/// "explicitly zero" — a 0-token call is a real signal, an unknown
/// is silent.
fn push_usage_attrs(attrs: &mut Vec<KeyValue>, usage: &GenAiUsage) {
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
        ] {
            assert!(keys.contains(&expected), "missing attr: {expected}");
        }
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
