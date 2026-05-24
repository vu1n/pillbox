//! OTel sinks — OTLP log records (per event) + spans (per
//! sandbox-side session) to whichever collector
//! `$OTEL_EXPORTER_OTLP_ENDPOINT` points at. Default transport is
//! HTTP/protobuf via the blocking reqwest client (no tokio runtime
//! drag — matches the webhook sink's sync model). Optional gRPC
//! behind the `otel-grpc` cargo feature.
//!
//! Spans are sandbox-only and only emitted at terminal events
//! (`session.completed` / `session.failed`) when both the OTel
//! endpoint AND the wrapper-captured `PILLBOX_SESSION_STARTED_AT`
//! env are set. Skipping span emission when start time is unknown
//! is intentional — zero-duration spans would be structurally
//! worse than no spans at all.

use std::{collections::HashMap, sync::OnceLock, time::SystemTime};

use anyhow::{Context, Result};
use opentelemetry::logs::{Logger, LoggerProvider};
use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanId, Status, TraceId, Tracer, TracerProvider,
};
use opentelemetry::Context as OtelContext;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::SdkLogger;
use opentelemetry_sdk::trace::SdkTracer;

use super::{session_started_at_from_env, AttrValue, Emitter, EventType, EVENTS_SINK_TIMEOUT};
use crate::url_safety::plaintext_non_loopback_host;

/// Cached per process so the TLS-context + exporter setup costs land
/// on the first event of the run, not every emit. The inner `Option`
/// is the "configured or not" flag: `None` means the env var was
/// unset at first call, so we skip the sink for the lifetime of the
/// process (env-var flipping mid-process isn't a supported workflow).
static OTEL_LOGGER: OnceLock<Option<SdkLogger>> = OnceLock::new();

/// Default `service.name` resource attribute when `OTEL_SERVICE_NAME`
/// isn't set. Spec-recommended fallback chain is OTEL_SERVICE_NAME →
/// OTEL_RESOURCE_ATTRIBUTES → "unknown_service"; pillbox is more
/// useful than `unknown_service:pillbox` so we hardcode it.
const OTEL_DEFAULT_SERVICE_NAME: &str = "pillbox";

/// Build one OTLP log record per event and emit through the cached
/// SDK logger. Returns `Ok(())` (best-effort skip) when the env var
/// isn't set; otherwise propagates exporter-build failures so the
/// caller's `warn_on_sink_error` can surface them. Consumes `attrs`
/// because this is the last sink to touch them.
pub(super) fn sink_emit(
    ty: &EventType,
    attrs: Vec<(&'static str, Option<AttrValue>)>,
) -> Result<()> {
    let Some(logger) = logger() else {
        return Ok(());
    };
    let mut record = logger.create_log_record();
    fill_log_record(&mut record, ty, attrs);
    logger.emit(record);
    Ok(())
}

/// Lazy-init the shared logger from env. Returns `None` if
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is unset/empty (the sink is opt-in;
/// most pillbox invocations won't have an OTel collector configured
/// and shouldn't pay for the SDK bootstrap). Build failures are
/// printed once and cached as `None` so a misconfigured endpoint
/// doesn't repeatedly spam stderr.
fn logger() -> Option<&'static SdkLogger> {
    OTEL_LOGGER
        .get_or_init(|| {
            let endpoint = resolve_signal_endpoint("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "v1/logs")?;
            let (service_name, headers) = read_otel_common_config(&endpoint);
            match build_otel_logger(&endpoint, headers, &service_name) {
                Ok(logger) => Some(logger),
                Err(e) => {
                    eprintln!(
                        "pillbox: warning: OTel exporter init failed for `{endpoint}`: {e:#}; \
                         continuing with other sinks."
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Shared prelude for the lazy-init logger and tracer paths: warn on
/// plaintext-to-non-loopback, read `OTEL_SERVICE_NAME` (default
/// `pillbox`), parse `OTEL_EXPORTER_OTLP_HEADERS`. Pulled out so a
/// future signal (metrics? events?) inherits the same posture
/// without copy-paste drift.
fn read_otel_common_config(endpoint: &str) -> (String, HashMap<String, String>) {
    warn_if_plaintext_to_non_loopback(endpoint);
    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| OTEL_DEFAULT_SERVICE_NAME.to_string());
    let headers = parse_otel_headers(
        std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
            .as_deref()
            .unwrap_or(""),
    );
    (service_name, headers)
}

/// Resolve an OTLP/HTTP endpoint per the spec. The signal-specific
/// env (`OTEL_EXPORTER_OTLP_{LOGS,TRACES,…}_ENDPOINT`) wins when set
/// — it's used verbatim because the user wanted explicit control
/// over the full URL. Otherwise we fall back to the shared base env
/// and tack on the `signal_path` (`v1/logs`, `v1/traces`, etc.) per
/// the spec's "base URL + signal" composition rule.
fn resolve_signal_endpoint(signal_env: &str, signal_path: &str) -> Option<String> {
    if let Ok(signal) = std::env::var(signal_env) {
        let trimmed = signal.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
    append_signal_path(&base, signal_path)
}

/// Append a signal path (e.g. `v1/logs`, `v1/traces`) to a base OTLP
/// URL with trailing-slash normalization. Pure (no env reads, no I/O)
/// so tests can pin the path-assembly behavior without racing the
/// global env table.
fn append_signal_path(base: &str, signal_path: &str) -> Option<String> {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("{trimmed}/{signal_path}"))
}

/// Parse one `OTEL_EXPORTER_OTLP_HEADERS` value into a header map.
/// Comma-separated `k=v` pairs per the OTLP spec. Percent-decoding
/// intentionally skipped — matches the Go and Python SDKs' default
/// behavior; values with literal commas are silently truncated, same
/// as those SDKs. Empty input returns an empty map.
fn parse_otel_headers(raw: &str) -> HashMap<String, String> {
    if raw.is_empty() {
        return HashMap::new();
    }
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// One-time warning when the configured collector is plaintext HTTP
/// to a non-loopback host. Event attributes can carry user-supplied
/// `label` text + session ids; in-cluster collectors over plain HTTP
/// are fine, but a remote cleartext endpoint is almost always a
/// misconfig. Mirrors the webhook sink's posture (same threat model,
/// same shared helper).
fn warn_if_plaintext_to_non_loopback(endpoint: &str) {
    if let Some(host) = plaintext_non_loopback_host(endpoint) {
        eprintln!(
            "pillbox: warning: OTel endpoint `{endpoint}` is plaintext HTTP to a non-loopback host \
             (`{host}`) — events include session ids + user-supplied labels. Prefer https:// for remote collectors."
        );
    }
}

/// Build the SDK logger for `endpoint`. The simple processor exports
/// inline on emit — no background runtime, no shutdown coordination —
/// which keeps the sink usable from sync code paths. The blocking
/// reqwest client (selected via the `reqwest-blocking-client`
/// feature on `opentelemetry-otlp`) matches.
///
/// The `SdkLoggerProvider` is dropped here, but `provider.logger(...)`
/// clones the provider's `Arc<inner>` into the returned `SdkLogger`,
/// so the processor + exporter stay alive for the cached logger's
/// lifetime. With simple processor there's no buffer to flush on
/// shutdown; if a future PR switches to batch processing, an
/// `at_exit` hook to call `provider.shutdown()` becomes load-bearing.
fn build_otel_logger(
    endpoint: &str,
    headers: HashMap<String, String>,
    service_name: &str,
) -> Result<SdkLogger> {
    let mut builder = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_timeout(EVENTS_SINK_TIMEOUT);
    if !headers.is_empty() {
        builder = builder.with_headers(headers);
    }
    let exporter = builder.build().context("build OTLP log exporter")?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();
    let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_simple_exporter(exporter)
        .build();
    Ok(provider.logger("pillbox"))
}

/// Cached tracer. Shape mirrors [`OTEL_LOGGER`]: `None` means the
/// sink is unconfigured (no endpoint, or build failed) and we skip
/// for the lifetime of the process.
static OTEL_TRACER: OnceLock<Option<SdkTracer>> = OnceLock::new();

/// Emit one span for a terminal sandbox-side event. The host emits
/// `session.dropped` after the sandbox is gone — the sandbox's
/// terminal event already closed the span, so the host doesn't
/// produce duplicate spans.
///
/// Returns `Ok(())` (best-effort skip) when any prerequisite is
/// unmet: not a terminal event, not sandbox-side, no endpoint
/// configured, no wrapper-captured start time. Tests for any of
/// these conditions short-circuit before touching the tracer cache.
pub(super) fn span_emit_if_terminal_sandbox(
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
        .with_span_id(derive_span_id(session_id))
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

fn tracer() -> Option<&'static SdkTracer> {
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
/// even though [`span_emit_if_terminal_sandbox`] short-circuits
/// before they reach us.
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
/// `None` entries. Same omit-vs-null rule as `fill_log_record`.
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
/// PR 2c.4 is "one span per session", so `trace_id` and `span_id`
/// both derive from the same session id — the trace tree has a
/// single span. If a future PR adds child spans (per-tool-call,
/// per-message), `span_id` will need a per-span derivation while
/// `trace_id` stays anchored to the session. Until then the symmetry
/// keeps correlation trivial.
fn derive_trace_id(session_id: &str) -> TraceId {
    TraceId::from_bytes(pack_id_bytes::<16>(session_id))
}

/// Same packing as [`derive_trace_id`] but for the 64-bit span id.
fn derive_span_id(session_id: &str) -> SpanId {
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

/// Fill a freshly-created [`LogRecord`] from the shared attribute
/// list. `None` values are omitted (OTel attribute bags have no
/// notion of "present with null value"). Kept separate from
/// [`sink_emit`] so the attribute-shape test can exercise it without
/// touching the global logger cache.
fn fill_log_record<R: opentelemetry::logs::LogRecord>(
    record: &mut R,
    ty: &EventType,
    attrs: Vec<(&'static str, Option<AttrValue>)>,
) {
    let (severity_number, severity_text) = severity_for(ty);
    record.set_severity_number(severity_number);
    record.set_severity_text(severity_text);
    record.set_event_name(ty.as_str());
    // OTel convention: `body` is the human-readable message; the
    // structured payload lives in attributes. Using the event name as
    // body keeps single-line tail-style log viewers readable without
    // duplicating the structured fields.
    record.set_body(opentelemetry::logs::AnyValue::String(ty.as_str().into()));
    for (key, value) in attrs {
        if let Some(v) = value {
            record.add_attribute(key, attr_to_otel(v));
        }
    }
}

fn attr_to_otel(v: AttrValue) -> opentelemetry::logs::AnyValue {
    match v {
        AttrValue::Str(s) => opentelemetry::logs::AnyValue::String(s.into()),
        AttrValue::Int(i) => opentelemetry::logs::AnyValue::Int(i),
    }
}

/// Map a lifecycle event to its (`Severity`, severity-text) pair.
/// Failed sessions are ERROR; everything else is INFO. The text label
/// is what shows up in human-readable views; the numeric severity is
/// what severity-based filters key on per the OTel logs spec.
fn severity_for(ty: &EventType) -> (opentelemetry::logs::Severity, &'static str) {
    match ty {
        EventType::SessionFailed { .. } => (opentelemetry::logs::Severity::Error, "ERROR"),
        _ => (opentelemetry::logs::Severity::Info, "INFO"),
    }
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

    use super::super::build_attributes;
    use crate::session::Session;

    #[test]
    fn severity_for_event_type() {
        // Failed sessions map to ERROR; everything else INFO. The
        // numeric severity is what severity-based filters key on per
        // the OTel logs spec; the text label is for human-readable
        // views.
        let (sev, text) = severity_for(&EventType::SessionFailed {
            reason: "x".into(),
            exit_code: None,
            trace_path: None,
            result_snapshot: None,
        });
        assert_eq!(sev, opentelemetry::logs::Severity::Error);
        assert_eq!(text, "ERROR");

        for ty in [
            EventType::SessionStarted {
                parent_session_id: None,
            },
            EventType::SessionCompleted {
                exit_code: None,
                trace_path: None,
                result_snapshot: None,
            },
            EventType::SessionDropped,
        ] {
            let (sev, text) = severity_for(&ty);
            assert_eq!(sev, opentelemetry::logs::Severity::Info, "{}", ty.as_str());
            assert_eq!(text, "INFO");
        }
    }

    #[test]
    fn append_signal_path_normalizes_trailing_slash() {
        // OTLP spec: OTEL_EXPORTER_OTLP_ENDPOINT is a BASE URL; the
        // signal path gets appended. Trailing-slash normalization so
        // `http://host:4318` and `http://host:4318/` produce the same
        // target. Empty input → None (no fallback URL).
        assert_eq!(
            append_signal_path("http://collector:4318", "v1/logs").as_deref(),
            Some("http://collector:4318/v1/logs")
        );
        assert_eq!(
            append_signal_path("http://collector:4318/", "v1/logs").as_deref(),
            Some("http://collector:4318/v1/logs")
        );
        // Traces signal — same composition logic; pinning a second
        // suffix proves the helper isn't accidentally specialized.
        assert_eq!(
            append_signal_path("http://collector:4318", "v1/traces").as_deref(),
            Some("http://collector:4318/v1/traces")
        );
        // A nonstandard base path (e.g. behind a reverse proxy with
        // a /otel prefix) gets the signal path appended too — that's
        // the spec; users wanting a custom path use the signal-
        // specific env var instead.
        assert_eq!(
            append_signal_path("http://collector/otel/", "v1/logs").as_deref(),
            Some("http://collector/otel/v1/logs")
        );
        assert_eq!(append_signal_path("   ", "v1/logs").as_deref(), None);
        assert_eq!(append_signal_path("", "v1/logs").as_deref(), None);
    }

    #[test]
    fn parse_otel_headers_handles_comma_separated_pairs() {
        let h = parse_otel_headers("authorization=Bearer abc, x-tenant=acme");
        assert_eq!(h.get("authorization"), Some(&"Bearer abc".to_string()));
        assert_eq!(h.get("x-tenant"), Some(&"acme".to_string()));
        assert_eq!(h.len(), 2);
        // Empty key dropped, trailing comma tolerated.
        let h = parse_otel_headers("=value, k=v,");
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("k"), Some(&"v".to_string()));
        // Empty input → empty map (matches unset-env behavior).
        assert!(parse_otel_headers("").is_empty());
    }

    #[test]
    fn otel_sink_posts_protobuf_to_logs_endpoint() {
        // End-to-end: build a logger pointing at a loopback HTTP
        // listener, emit a record, verify the listener received a
        // POST to /v1/logs with the OTel protobuf content-type. We
        // don't decode the protobuf body — the SDK owns that shape
        // and re-implementing parser-level assertions here would
        // duplicate its tests. What we DO care about: pillbox is
        // the one calling into the SDK, and a regression where we
        // forgot to flush or routed to the wrong path needs to
        // surface here.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        // Full signal-specific URL — what `resolve_signal_endpoint`
        // produces in production when `OTEL_EXPORTER_OTLP_ENDPOINT`
        // is the base URL. Passing the bare base would post to `/`
        // (the SDK trusts our endpoint to be the final target).
        let endpoint = format!("http://{addr}/v1/logs");

        let received: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let recv_clone = Arc::clone(&received);
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            // Read until we see a blank line (end of headers) plus
            // the declared Content-Length, OR the buffer fills. The
            // OTel exporter sends a single POST per emit so one read
            // is enough for the shape check.
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            // 200 with empty body — the OTLP/HTTP spec lets the
            // collector reply with an empty ExportLogsServiceResponse
            // protobuf when everything succeeded, which is what
            // collectors do in practice.
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            *recv_clone.lock().unwrap() = Some(request);
        });

        let logger = build_otel_logger(&endpoint, HashMap::new(), "pillbox-test")
            .expect("build OTel logger");
        let mut record = logger.create_log_record();
        let session = Session::test_fixture();
        let ty = EventType::SessionStarted {
            parent_session_id: None,
        };
        let attrs = build_attributes(&ty, "abc123def456", Some(&session));
        fill_log_record(&mut record, &ty, attrs);
        logger.emit(record);

        server.join().expect("server thread");
        let req = received.lock().unwrap().take().expect("got request");
        assert!(
            req.starts_with("POST /v1/logs"),
            "expected POST /v1/logs, got: {req}"
        );
        assert!(
            req.to_lowercase()
                .contains("content-type: application/x-protobuf"),
            "expected OTLP protobuf content-type, got: {req}"
        );
    }

    #[test]
    fn derive_ids_are_stable_across_calls() {
        // Span id correlation depends on host's and sandbox's derive
        // calls producing the same bytes for the same session id;
        // pin that here so a future refactor of `pack_id_bytes` can't
        // silently break cross-emitter span stitching.
        let a = derive_span_id("abc123def456");
        let b = derive_span_id("abc123def456");
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        let t1 = derive_trace_id("abc123def456");
        let t2 = derive_trace_id("abc123def456");
        assert_eq!(format!("{t1:?}"), format!("{t2:?}"));
        // Different sessions → different ids (sanity check that the
        // packing isn't trivially folding everything to zero).
        assert_ne!(
            format!("{:?}", derive_span_id("abc123def456")),
            format!("{:?}", derive_span_id("111122223333"))
        );
    }

    #[test]
    fn derive_ids_handle_non_hex_session_ids() {
        // `validate_session_id` accepts alphanumeric + `-`; non-hex
        // input falls through to the FNV-1a path. We don't pin the
        // exact bytes (the hash function is an implementation
        // detail) — only that the result is stable and non-zero so
        // OTel doesn't reject the id as invalid.
        let span = derive_span_id("ZZZZyyyy----");
        let again = derive_span_id("ZZZZyyyy----");
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
    fn span_emit_skips_non_terminal_events() {
        // `session.started` and `session.dropped` are not span-
        // terminal — the sandbox-side `session done` closes the span.
        // Confirm the early-return short-circuits before touching the
        // tracer cache (the function returns Ok regardless of any
        // env state).
        let attrs: Vec<(&'static str, Option<AttrValue>)> = vec![];
        let res = span_emit_if_terminal_sandbox(
            &EventType::SessionStarted {
                parent_session_id: None,
            },
            "abc123def456",
            &attrs,
            Emitter::Sandbox,
        );
        assert!(res.is_ok());
        let res = span_emit_if_terminal_sandbox(
            &EventType::SessionDropped,
            "abc123def456",
            &attrs,
            Emitter::Sandbox,
        );
        assert!(res.is_ok());
    }

    #[test]
    fn span_emit_skips_host_emitter() {
        // Host's session done (orchestrator-driven completion) doesn't
        // own a span — the sandbox-side emit already closed it.
        let attrs: Vec<(&'static str, Option<AttrValue>)> = vec![];
        let res = span_emit_if_terminal_sandbox(
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
            .with_span_id(derive_span_id("abc123def456"))
            .with_start_time(span_start)
            .with_end_time(SystemTime::now())
            .with_status(Status::Ok);
        let mut span = tracer.build_with_context(builder, &OtelContext::new());
        use opentelemetry::trace::Span as _;
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
