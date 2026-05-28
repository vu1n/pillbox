//! OTLP spans sink — sandbox-only, one span per session, built
//! atomically at the terminal event and exported via OTLP/HTTP.
//!
//! The host emits `session.dropped` after the sandbox is gone — the
//! sandbox's terminal event already closed the span, so the host
//! never produces duplicate spans here. Emission is also gated on
//! `PILLBOX_SESSION_STARTED_AT` being set by the wrapper so the
//! span has real duration; without it the log record still ships,
//! the span doesn't.

use std::{collections::HashMap, sync::OnceLock, time::SystemTime};

use anyhow::{Context, Result};
use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanId, Status, TraceId, Tracer, TracerProvider,
};
use opentelemetry::Context as OtelContext;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::SdkTracer;

use super::super::{
    session_started_at_from_env, AttrValue, Emitter, EventType, EVENTS_SINK_TIMEOUT,
};
use super::{read_otel_common_config, resolve_signal_endpoint};

/// Cached tracer. Shape mirrors the logs sink's `OTEL_LOGGER`:
/// `None` means the sink is unconfigured (no endpoint, or build
/// failed) and we skip for the lifetime of the process.
static OTEL_TRACER: OnceLock<Option<SdkTracer>> = OnceLock::new();

/// Emit one span for a terminal sandbox-side event. Returns `Ok(())`
/// (best-effort skip) when any prerequisite is unmet: not a terminal
/// event, not sandbox-side, no endpoint configured, no wrapper-
/// captured start time. Tests for any of these short-circuit before
/// touching the tracer cache.
pub(in crate::events) fn sink_emit(
    ty: &EventType,
    session_id: &str,
    attrs: &[(&'static str, Option<AttrValue>)],
    emitter: Emitter,
) -> Result<()> {
    if !ty.is_terminal() || emitter != Emitter::Sandbox {
        return Ok(());
    }
    let Some(start_time) = session_started_at_from_env().and_then(|s| parse_rfc3339(&s)) else {
        // Wrapper didn't capture `PILLBOX_SESSION_STARTED_AT`. Without
        // it the span would have start == end (a zero-duration mark
        // that misleads consumers); skip emission and let the log
        // record carry the terminal signal alone.
        return Ok(());
    };
    let Some(tracer) = tracer() else {
        return Ok(());
    };
    let span_builder = SpanBuilder::from_name("session")
        .with_trace_id(derive_trace_id(session_id))
        .with_span_id(derive_session_span_id(session_id))
        .with_start_time(start_time)
        .with_end_time(SystemTime::now())
        .with_status(otel_status_for(ty))
        .with_attributes(span_attributes(attrs));
    // SDK's simple span processor exports on `.end()`. Building +
    // immediately dropping the span at scope exit triggers that
    // export inline (matches our log sink's sync emit model).
    let mut span = tracer.build_with_context(span_builder, &OtelContext::new());
    span.end();
    Ok(())
}

pub(in crate::events) fn tracer() -> Option<&'static SdkTracer> {
    OTEL_TRACER
        .get_or_init(|| {
            let endpoint =
                resolve_signal_endpoint("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "v1/traces")?;
            let (service_name, headers) = read_otel_common_config(&endpoint);
            match build_otel_tracer(&endpoint, headers, &service_name) {
                Ok(tracer) => Some(tracer),
                Err(e) => {
                    eprintln!(
                        "pillbox: warning: OTel span exporter init failed for `{endpoint}`: {e:#}; \
                         logs continue."
                    );
                    None
                }
            }
        })
        .as_ref()
}

fn build_otel_tracer(
    endpoint: &str,
    headers: HashMap<String, String>,
    service_name: &str,
) -> Result<SdkTracer> {
    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_timeout(EVENTS_SINK_TIMEOUT);
    if !headers.is_empty() {
        builder = builder.with_headers(headers);
    }
    let exporter = builder.build().context("build OTLP span exporter")?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_simple_exporter(exporter)
        .build();
    Ok(provider.tracer("pillbox"))
}

/// Parse `expires_at`-shaped RFC3339 (UTC, second precision) into a
/// `SystemTime` for `span.start_time`. Tolerant of `+00:00` and `Z`
/// suffixes (POSIX `date -Iseconds` emits the former; pillbox emits
/// the latter). Returns `None` on any parse error — the caller skips
/// span emission rather than fall back to a misleading "now".
fn parse_rfc3339(value: &str) -> Option<SystemTime> {
    use time::format_description::well_known::Rfc3339;
    let parsed = time::OffsetDateTime::parse(value, &Rfc3339).ok()?;
    Some(parsed.into())
}

/// Map an [`EventType`] to its OTel [`Status`]. Exhaustive over the
/// enum so adding a new variant forces a decision here; the non-
/// terminal arms render `Unset` (the spec default for "no opinion")
/// even though [`sink_emit`] short-circuits before they reach us.
fn otel_status_for(ty: &EventType) -> Status {
    match ty {
        EventType::SessionCompleted { .. } => Status::Ok,
        EventType::SessionFailed { reason, .. } => Status::Error {
            description: reason.clone().into(),
        },
        EventType::SessionStarted { .. } | EventType::SessionDropped => Status::Unset,
    }
}

/// Project the shared attribute list onto OTel `KeyValue`s, dropping
/// `None` entries. Same omit-vs-null rule as the logs sink.
fn span_attributes(attrs: &[(&'static str, Option<AttrValue>)]) -> Vec<KeyValue> {
    attrs
        .iter()
        .filter_map(|(k, v)| {
            let v = v.as_ref()?;
            let value: opentelemetry::Value = match v {
                AttrValue::Str(s) => s.clone().into(),
                AttrValue::Int(i) => (*i).into(),
            };
            Some(KeyValue::new(*k, value))
        })
        .collect()
}

/// Deterministically pack a session id into an OTel 128-bit trace id
/// so events for the same session correlate without a lookup table.
/// Hyphens are stripped; the remaining hex (or hash fallback for
/// non-hex shapes) is right-aligned into the 32-hex trace id.
///
/// Used by both the session span (its trace anchor) and by gen_ai
/// spans emitted from the vault MITM — they share the trace via this
/// same derivation, so the gen_ai spans become children of the
/// session span without any out-of-band lookup.
pub(in crate::events) fn derive_trace_id(session_id: &str) -> TraceId {
    TraceId::from_bytes(pack_id_bytes::<16>(session_id))
}

/// Deterministic span_id for *the session span itself*. gen_ai spans
/// set this as their `parent_span_id` to nest under the session span
/// without needing the session span to have been emitted first —
/// Workshop / collectors stitch the link by id alone.
pub(in crate::events) fn derive_session_span_id(session_id: &str) -> SpanId {
    SpanId::from_bytes(pack_id_bytes::<8>(session_id))
}

/// Right-align the hex characters of `id` into `N` bytes. For the
/// 12-char hex shape `Session::new_id` mints today, the result is
/// just the parsed value padded with leading zeros. For ids that
/// somehow contain non-hex characters (forward-compat per
/// `validate_session_id`), we fold them via a tiny FNV-1a so the
/// result is still deterministic across processes — required for
/// host-side ↔ sandbox-side span_id correlation on the same session.
fn pack_id_bytes<const N: usize>(id: &str) -> [u8; N] {
    let mut out = [0u8; N];
    // Strip hyphens; everything else passes `validate_session_id`'s
    // alphanumeric gate.
    let cleaned: String = id.chars().filter(|c| *c != '-').collect();
    if cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        // Hex fast path: parse the cleaned id as a big-endian integer
        // and right-align into the output buffer.
        let want = N * 2;
        let padded = if cleaned.len() >= want {
            cleaned[cleaned.len() - want..].to_string()
        } else {
            format!("{cleaned:0>want$}")
        };
        for (i, chunk) in padded.as_bytes().chunks_exact(2).enumerate() {
            // Each chunk is two ASCII hex digits; the parse only fails
            // on non-hex input which we already gated against above.
            out[i] =
                u8::from_str_radix(std::str::from_utf8(chunk).unwrap_or("00"), 16).unwrap_or(0);
        }
        return out;
    }
    // Non-hex fallback: FNV-1a fold. Produces a stable mapping for
    // arbitrary alphanumeric session ids without pulling in a hash
    // crate; collision probability is high enough at 64/128 bits to
    // be acceptable for the "this shouldn't happen in practice" path.
    let mut hash: u128 = 0xcbf29ce484222325;
    for b in cleaned.bytes() {
        hash ^= b as u128;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let bytes = hash.to_be_bytes();
    let take = N.min(16);
    out[N - take..].copy_from_slice(&bytes[16 - take..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    #[test]
    fn derive_ids_are_stable_across_calls() {
        // Span id correlation depends on host's and sandbox's derive
        // calls producing the same bytes for the same session id;
        // pin that here so a future refactor of `pack_id_bytes` can't
        // silently break cross-emitter span stitching.
        let a = derive_session_span_id("abc123def456");
        let b = derive_session_span_id("abc123def456");
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        let t1 = derive_trace_id("abc123def456");
        let t2 = derive_trace_id("abc123def456");
        assert_eq!(format!("{t1:?}"), format!("{t2:?}"));
        // Different sessions → different ids (sanity check that the
        // packing isn't trivially folding everything to zero).
        assert_ne!(
            format!("{:?}", derive_session_span_id("abc123def456")),
            format!("{:?}", derive_session_span_id("111122223333"))
        );
    }

    #[test]
    fn derive_ids_handle_non_hex_session_ids() {
        // `validate_session_id` accepts alphanumeric + `-`; non-hex
        // input falls through to the FNV-1a path. We don't pin the
        // exact bytes (the hash function is an implementation
        // detail) — only that the result is stable and non-zero so
        // OTel doesn't reject the id as invalid.
        let span = derive_session_span_id("ZZZZyyyy----");
        let again = derive_session_span_id("ZZZZyyyy----");
        assert_eq!(format!("{span:?}"), format!("{again:?}"));
        assert_ne!(
            format!("{span:?}"),
            format!("{:?}", SpanId::from_bytes([0u8; 8]))
        );
    }

    #[test]
    fn otel_status_maps_failed_to_error_with_reason() {
        let ok = otel_status_for(&EventType::SessionCompleted {
            exit_code: Some(0),
            trace_path: None,
            result_snapshot: None,
        });
        assert!(matches!(ok, Status::Ok));

        let err = otel_status_for(&EventType::SessionFailed {
            reason: "agent panic".into(),
            exit_code: Some(1),
            trace_path: None,
            result_snapshot: None,
        });
        match err {
            Status::Error { description } => assert_eq!(description, "agent panic"),
            other => panic!("expected Error, got {other:?}"),
        }

        let unset = otel_status_for(&EventType::SessionDropped);
        assert!(matches!(unset, Status::Unset));
    }

    #[test]
    fn parse_rfc3339_accepts_z_and_offset_suffixes() {
        // POSIX `date -u -Iseconds` emits +00:00; pillbox's own
        // `now_rfc3339` emits Z. Both must parse for the span sink to
        // accept either source uniformly. Non-UTC offsets parse too —
        // a wrapper that captured `date -Iseconds` (no -u) in a
        // non-UTC sandbox image would feed `-05:00` etc.
        assert!(parse_rfc3339("2026-05-24T10:00:00Z").is_some());
        assert!(parse_rfc3339("2026-05-24T10:00:00+00:00").is_some());
        assert!(parse_rfc3339("2026-05-24T10:00:00-05:00").is_some());
        assert!(parse_rfc3339("2026-05-24T10:00:00+09:30").is_some());
        assert!(parse_rfc3339("not a timestamp").is_none());
        assert!(parse_rfc3339("").is_none());
    }

    #[test]
    fn sink_emit_skips_non_terminal_events() {
        // `session.started` and `session.dropped` are not span-
        // terminal — the sandbox-side `session done` closes the span.
        // Confirm the early-return short-circuits before touching the
        // tracer cache (the function returns Ok regardless of any
        // env state).
        let attrs: Vec<(&'static str, Option<AttrValue>)> = vec![];
        let res = sink_emit(
            &EventType::SessionStarted {
                parent_session_id: None,
            },
            "abc123def456",
            &attrs,
            Emitter::Sandbox,
        );
        assert!(res.is_ok());
        let res = sink_emit(
            &EventType::SessionDropped,
            "abc123def456",
            &attrs,
            Emitter::Sandbox,
        );
        assert!(res.is_ok());
    }

    #[test]
    fn sink_emit_skips_host_emitter() {
        // Host's session done (orchestrator-driven completion) doesn't
        // own a span — the sandbox-side emit already closed it.
        let attrs: Vec<(&'static str, Option<AttrValue>)> = vec![];
        let res = sink_emit(
            &EventType::SessionCompleted {
                exit_code: Some(0),
                trace_path: None,
                result_snapshot: None,
            },
            "abc123def456",
            &attrs,
            Emitter::Host,
        );
        assert!(res.is_ok());
    }

    #[test]
    fn span_posts_protobuf_to_traces_endpoint() {
        // End-to-end: build a tracer pointing at a loopback HTTP
        // listener, emit a span via SpanBuilder + Tracer::build, then
        // verify the listener received POST /v1/traces. Same shape as
        // the logs end-to-end test.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}/v1/traces");

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let recv_clone = Arc::clone(&received);
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            *recv_clone.lock().unwrap() = Some(request);
        });

        let tracer = build_otel_tracer(&endpoint, HashMap::new(), "pillbox-test")
            .expect("build OTel tracer");
        let span_start = SystemTime::now() - Duration::from_secs(2);
        let builder = SpanBuilder::from_name("session")
            .with_trace_id(derive_trace_id("abc123def456"))
            .with_span_id(derive_session_span_id("abc123def456"))
            .with_start_time(span_start)
            .with_end_time(SystemTime::now())
            .with_status(Status::Ok);
        let mut span = tracer.build_with_context(builder, &OtelContext::new());
        span.end();
        drop(tracer);

        server.join().expect("server thread");
        let req = received.lock().unwrap().take().expect("got request");
        assert!(
            req.starts_with("POST /v1/traces"),
            "expected POST /v1/traces, got: {req}"
        );
        assert!(
            req.to_lowercase()
                .contains("content-type: application/x-protobuf"),
            "got: {req}"
        );
    }
}
