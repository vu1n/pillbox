//! OTLP logs sink — one log record per lifecycle event. Always-on
//! once `OTEL_EXPORTER_OTLP_ENDPOINT` is configured; emits regardless
//! of emitter side or terminal-ness so consumers see the full
//! lifecycle stream as it happens.

use std::{collections::HashMap, sync::OnceLock};

use anyhow::{Context, Result};
use opentelemetry::logs::{Logger, LoggerProvider};
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::SdkLogger;

use super::super::{AttrValue, EventType, EVENTS_SINK_TIMEOUT};
use super::{read_otel_common_config, resolve_signal_endpoint};

/// Cached per process so the TLS-context + exporter setup costs land
/// on the first event of the run, not every emit. The inner `Option`
/// is the "configured or not" flag: `None` means the env var was
/// unset at first call, so we skip the sink for the lifetime of the
/// process (env-var flipping mid-process isn't a supported workflow).
static OTEL_LOGGER: OnceLock<Option<SdkLogger>> = OnceLock::new();

/// Build one OTLP log record per event and emit through the cached
/// SDK logger. Returns `Ok(())` (best-effort skip) when the env var
/// isn't set; otherwise propagates exporter-build failures so the
/// caller's `warn_on_sink_error` can surface them. Consumes `attrs`
/// because this is the last sink to touch them.
pub(in crate::events) fn sink_emit(
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
        AttrValue::Json(j) => opentelemetry::logs::AnyValue::String(j.to_string().into()),
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

    use super::super::super::build_attributes;
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
                startup: None,
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
            startup: None,
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
