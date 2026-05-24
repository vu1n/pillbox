//! OTel logs sink — emits one OTLP log record per lifecycle event
//! to whichever collector `$OTEL_EXPORTER_OTLP_ENDPOINT` points at.
//! Default transport is HTTP/protobuf via the blocking reqwest
//! client (no tokio runtime drag — matches the webhook sink's sync
//! model). Optional gRPC behind the `otel-grpc` cargo feature.
//!
//! Spans land in the v0.7 PR 2c follow-up; emitting zero-duration
//! spans on the current four-event taxonomy would be structurally
//! worse than no spans at all.

use std::{collections::HashMap, sync::OnceLock};

use anyhow::{Context, Result};
use opentelemetry::logs::{Logger, LoggerProvider};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::SdkLogger;

use super::{AttrValue, EventType, EVENTS_SINK_TIMEOUT};
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
            let endpoint = resolve_logs_endpoint()?;
            warn_if_plaintext_to_non_loopback(&endpoint);
            let service_name = std::env::var("OTEL_SERVICE_NAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| OTEL_DEFAULT_SERVICE_NAME.to_string());
            let headers = parse_otel_headers(
                std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
                    .as_deref()
                    .unwrap_or(""),
            );
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

/// Resolve the OTLP/HTTP logs endpoint per the spec. Reads env once,
/// hands the base URL to [`format_logs_endpoint`] for the signal-path
/// append. The signal-specific env (`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`)
/// wins per the OTLP spec — when set, it's used verbatim because the
/// user wanted explicit control over the full URL.
fn resolve_logs_endpoint() -> Option<String> {
    if let Ok(signal) = std::env::var("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT") {
        let trimmed = signal.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let base = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
    format_logs_endpoint(&base)
}

/// Append `/v1/logs` to a base OTLP URL with trailing-slash
/// normalization. Pure (no env reads, no I/O) so the test can pin the
/// path-assembly behavior without racing the global env table.
fn format_logs_endpoint(base: &str) -> Option<String> {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("{trimmed}/v1/logs"))
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
    fn format_logs_endpoint_appends_v1_logs_to_base() {
        // OTLP spec: OTEL_EXPORTER_OTLP_ENDPOINT is a BASE URL; the
        // signal path gets appended. Trailing-slash normalization so
        // `http://host:4318` and `http://host:4318/` produce the same
        // target. Empty input → None (no fallback URL).
        assert_eq!(
            format_logs_endpoint("http://collector:4318").as_deref(),
            Some("http://collector:4318/v1/logs")
        );
        assert_eq!(
            format_logs_endpoint("http://collector:4318/").as_deref(),
            Some("http://collector:4318/v1/logs")
        );
        // A nonstandard base path (e.g. behind a reverse proxy with
        // a /otel prefix) gets the signal path appended too — that's
        // the spec; users wanting a custom path use the signal-
        // specific env var instead.
        assert_eq!(
            format_logs_endpoint("http://collector/otel/").as_deref(),
            Some("http://collector/otel/v1/logs")
        );
        assert_eq!(format_logs_endpoint("   ").as_deref(), None);
        assert_eq!(format_logs_endpoint("").as_deref(), None);
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
        // Full signal-specific URL — what `resolve_logs_endpoint`
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
}
