//! OTel sinks — OTLP log records (per event) + spans (per
//! sandbox-side session) to whichever collector
//! `$OTEL_EXPORTER_OTLP_ENDPOINT` points at. Default transport is
//! HTTP/protobuf via the blocking reqwest client (no tokio runtime
//! drag — matches the webhook sink's sync model). Optional gRPC
//! behind the `otel-grpc` cargo feature.
//!
//! Each OTLP signal lives in its own submodule:
//!
//!  - [`logs`] — always-on (when endpoint configured). One log
//!    record per event, regardless of emitter side or terminal-ness.
//!  - [`spans`] — sandbox-only, terminal-only. One span per session,
//!    gated on `PILLBOX_SESSION_STARTED_AT` being set by the
//!    wrapper so `span.start_time` is meaningful. Without it the
//!    log record still ships, the span doesn't.
//!
//! Shared concerns (endpoint resolution, header parsing, plaintext
//! warning, service-name/headers prelude) stay here in `mod.rs` so a
//! future signal (metrics? span events?) inherits them by importing
//! from `super`.

use std::collections::HashMap;

use crate::url_safety::plaintext_non_loopback_host;

pub(super) mod logs;
pub(super) mod spans;

pub(super) use logs::sink_emit as log_sink_emit;
pub(super) use spans::sink_emit as span_sink_emit;

/// Default `service.name` resource attribute when `OTEL_SERVICE_NAME`
/// isn't set. Spec-recommended fallback chain is OTEL_SERVICE_NAME →
/// OTEL_RESOURCE_ATTRIBUTES → "unknown_service"; pillbox is more
/// useful than `unknown_service:pillbox` so we hardcode it.
const OTEL_DEFAULT_SERVICE_NAME: &str = "pillbox";

/// Shared prelude for the lazy-init logger and tracer paths: warn on
/// plaintext-to-non-loopback, read `OTEL_SERVICE_NAME` (default
/// `pillbox`), parse `OTEL_EXPORTER_OTLP_HEADERS`. Pulled out so a
/// future signal (metrics? events?) inherits the same posture
/// without copy-paste drift.
pub(super) fn read_otel_common_config(endpoint: &str) -> (String, HashMap<String, String>) {
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
pub(super) fn resolve_signal_endpoint(signal_env: &str, signal_path: &str) -> Option<String> {
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
pub(super) fn append_signal_path(base: &str, signal_path: &str) -> Option<String> {
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
pub(super) fn parse_otel_headers(raw: &str) -> HashMap<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
