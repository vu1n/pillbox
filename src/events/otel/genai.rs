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
    /// `model` field from the request body, when the body was a
    /// POST and parsed as JSON containing a string `"model"`. None
    /// for non-POSTs and for endpoints that don't carry a model.
    pub(crate) request_model: Option<String>,
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
    let span_name = match call.request_model.as_deref() {
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
    if let Some(model) = call.request_model.as_deref() {
        attrs.push(KeyValue::new("gen_ai.request.model", model.to_string()));
    }
    attrs
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
            request_model: Some("claude-sonnet-4-5-20250929".into()),
        }
    }

    #[test]
    fn build_attributes_includes_genai_semconv_keys() {
        let attrs = build_attributes(&sample_call());
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        assert!(keys.contains(&"gen_ai.system"));
        assert!(keys.contains(&"gen_ai.operation.name"));
        assert!(keys.contains(&"gen_ai.request.model"));
        assert!(keys.contains(&"server.address"));
        assert!(keys.contains(&"http.request.method"));
        assert!(keys.contains(&"http.response.status_code"));
        assert!(keys.contains(&"pillbox.sandbox_id"));
    }

    #[test]
    fn build_attributes_omits_model_when_absent() {
        let mut call = sample_call();
        call.request_model = None;
        let attrs = build_attributes(&call);
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        assert!(!keys.contains(&"gen_ai.request.model"));
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
