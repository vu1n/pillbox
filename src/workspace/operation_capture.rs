//! Bounded OpenDAL HTTP-attempt capture for the managed R2 workspace path.
//!
//! The observer sits below OpenDAL's retry layer. It records one safe,
//! redacted record for every invocation of `HttpFetch`, while response byte
//! counts are updated lazily as the returned `HttpBody` is consumed. The
//! capture is diagnostic evidence only: it never changes rustic's logical
//! byte counts or pretends to be provider billing data.

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use anyhow::{Context as AnyhowContext, Result};
use futures_util::Stream;
use http::{Method, Request, Response, StatusCode};
use opendal::{
    raw::oio::ReadDyn,
    raw::{HttpBody, HttpClient, HttpFetch},
    Buffer,
};
use serde::Serialize;
use uuid::Uuid;

const MAX_RECORDS: usize = 256;
const MAX_SELECTOR_BYTES: usize = 1024;
const MAX_REASONS: usize = 10;
// JSON numbers are exactly representable by the worker/runtime only through
// the IEEE-754 safe-integer ceiling. Capture counters never emit a larger
// value; crossing it marks the capture incomplete instead.
const MAX_JSON_SAFE: u64 = (1u64 << 53) - 1;

const REASON_RECORD_LIMIT: &str = "record_limit";
const REASON_UNKNOWN_ACTION: &str = "unknown_action";
const REASON_FOREIGN_SELECTOR: &str = "foreign_selector";
const REASON_TRANSPORT_ERROR: &str = "transport_error";
const REASON_REDIRECT: &str = "redirect";
const REASON_RESPONSE_ERROR: &str = "response_error";
const REASON_RESPONSE_INCOMPLETE: &str = "response_incomplete";
const REASON_OPERATION_FAILED: &str = "operation_failed";
const REASON_COUNTER_OVERFLOW: &str = "counter_overflow";

/// The operation owning one capture. This value is part of the capture-v1
/// wire contract and is deliberately closed rather than caller-defined.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaptureOperation {
    SnapshotRestore,
    SnapshotFinalize,
    Verification,
}

/// Capture-v1's terminal status.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CaptureStatus {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CaptureRecord {
    pub(crate) id: String,
    pub(crate) method: String,
    pub(crate) action: String,
    pub(crate) selector: Option<String>,
    pub(crate) status: Option<u16>,
    pub(crate) request_body_bytes: u64,
    pub(crate) response_body_bytes: u64,
    pub(crate) response_complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CaptureDocument {
    pub(crate) schema_version: u8,
    pub(crate) capture_type: &'static str,
    pub(crate) capture_id: String,
    pub(crate) operation: CaptureOperation,
    pub(crate) bucket: String,
    pub(crate) prefix: String,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) status: CaptureStatus,
    pub(crate) reasons: Vec<String>,
    pub(crate) records: Vec<CaptureRecord>,
}

#[derive(Debug)]
struct CaptureState {
    document: CaptureDocument,
    next_sequence: u64,
    finished: bool,
}

/// Shared mutable capture state used by the HTTP fetcher and lazy response
/// streams. Cloning a handle does not duplicate or widen the capture.
#[derive(Clone, Debug)]
pub(crate) struct CaptureHandle {
    state: Arc<Mutex<CaptureState>>,
}

impl CaptureHandle {
    pub(crate) fn new(operation: CaptureOperation, bucket: &str, prefix: &str) -> Self {
        Self {
            state: Arc::new(Mutex::new(CaptureState {
                document: CaptureDocument {
                    schema_version: 1,
                    capture_type: "r2_http_operation",
                    capture_id: Uuid::now_v7().to_string(),
                    operation,
                    bucket: bucket.to_string(),
                    prefix: normalize_capture_prefix(prefix),
                    started_at: crate::session::now_rfc3339(),
                    // The document is only emitted after `finish`, but a
                    // valid timestamp here keeps an interrupted sidecar
                    // useful if the process is terminated before finalization.
                    finished_at: crate::session::now_rfc3339(),
                    status: CaptureStatus::Incomplete,
                    reasons: Vec::new(),
                    records: Vec::new(),
                },
                next_sequence: 1,
                finished: false,
            })),
        }
    }

    /// Build an OpenDAL HTTP client whose transport retries and redirects are
    /// disabled. OpenDAL's own retry layer remains outside this client and
    /// therefore invokes it once for every actual attempt.
    pub(crate) fn http_client(&self) -> Result<HttpClient> {
        let client = reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build capture HTTP client")?;
        Ok(HttpClient::with(CaptureHttpFetch {
            inner: client,
            capture: self.clone(),
        }))
    }

    pub(crate) fn mark_capture_unavailable(&self) {
        self.mark_reason("capture_unavailable");
    }

    /// Mark the owning rustic operation as successful or failed and take a
    /// stable snapshot suitable for JSON or a private sidecar.
    pub(crate) fn finish(&self, success: bool) -> CaptureDocument {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.finished {
            return state.document.clone();
        }

        if !success {
            add_reason(&mut state.document.reasons, REASON_OPERATION_FAILED);
        }
        let has_incomplete = state
            .document
            .records
            .iter()
            .any(|record| !record.response_complete);
        let has_transport_error = state
            .document
            .records
            .iter()
            .any(|record| record.status.is_none());
        if has_incomplete {
            add_reason(&mut state.document.reasons, REASON_RESPONSE_INCOMPLETE);
        }
        if has_transport_error {
            add_reason(&mut state.document.reasons, REASON_TRANSPORT_ERROR);
        }
        state.document.finished_at = crate::session::now_rfc3339();
        let complete = success
            && state.document.reasons.is_empty()
            && state.document.records.iter().all(|record| {
                record.status.is_some()
                    && record.response_complete
                    && record.action != "unknown"
                    && record.selector.is_some()
            });
        state.document.status = if complete {
            CaptureStatus::Complete
        } else {
            CaptureStatus::Incomplete
        };
        state.finished = true;
        state.document.clone()
    }

    fn describe_request(&self, req: &Request<Buffer>) -> ParsedRequest {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let bucket = state.document.bucket.clone();
        let prefix = state.document.prefix.clone();
        drop(state);
        describe_request(req, &bucket, &prefix)
    }

    fn begin_attempt(&self, parsed: ParsedRequest, request_body_bytes: usize) -> Option<Attempt> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let request_body_bytes = match u64::try_from(request_body_bytes) {
            Ok(value) if value <= MAX_JSON_SAFE => value,
            Ok(_) => {
                add_reason(&mut state.document.reasons, REASON_COUNTER_OVERFLOW);
                MAX_JSON_SAFE
            }
            Err(_) => {
                add_reason(&mut state.document.reasons, REASON_COUNTER_OVERFLOW);
                MAX_JSON_SAFE
            }
        };
        if state.document.records.len() >= MAX_RECORDS {
            add_reason(&mut state.document.reasons, REASON_RECORD_LIMIT);
            return None;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let record = CaptureRecord {
            id: format!("{}:{sequence}", state.document.capture_id),
            method: parsed.method,
            action: parsed.action,
            selector: parsed.selector,
            status: None,
            request_body_bytes,
            response_body_bytes: 0,
            response_complete: false,
        };
        if record.action == "unknown" {
            add_reason(&mut state.document.reasons, REASON_UNKNOWN_ACTION);
        }
        if parsed.foreign_selector {
            add_reason(&mut state.document.reasons, REASON_FOREIGN_SELECTOR);
        }
        let index = state.document.records.len();
        state.document.records.push(record);
        Some(Attempt {
            capture: self.clone(),
            index,
        })
    }

    fn mark_reason(&self, reason: &'static str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        add_reason(&mut state.document.reasons, reason);
    }

    fn set_status(&self, index: usize, status: StatusCode) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(record) = state.document.records.get_mut(index) {
            record.status = Some(status.as_u16());
            if status.is_redirection() {
                add_reason(&mut state.document.reasons, REASON_REDIRECT);
            } else if status.is_client_error() || status.is_server_error() {
                add_reason(&mut state.document.reasons, REASON_RESPONSE_ERROR);
            }
        }
    }

    fn add_response_bytes(&self, index: usize, amount: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let amount = match u64::try_from(amount) {
            Ok(value) if value <= MAX_JSON_SAFE => value,
            Ok(_) => {
                add_reason(&mut state.document.reasons, REASON_COUNTER_OVERFLOW);
                MAX_JSON_SAFE
            }
            Err(_) => {
                add_reason(&mut state.document.reasons, REASON_COUNTER_OVERFLOW);
                MAX_JSON_SAFE
            }
        };
        if let Some(record) = state.document.records.get_mut(index) {
            let next = record
                .response_body_bytes
                .checked_add(amount)
                .filter(|value| *value <= MAX_JSON_SAFE);
            record.response_body_bytes = next.unwrap_or(MAX_JSON_SAFE);
            if next.is_none() {
                add_reason(&mut state.document.reasons, REASON_COUNTER_OVERFLOW);
            }
        }
    }

    fn mark_complete(&self, index: usize) {
        if let Some(record) = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .document
            .records
            .get_mut(index)
        {
            record.response_complete = true;
        }
    }

    fn mark_response_error(&self, _index: usize) {
        self.mark_reason(REASON_RESPONSE_ERROR);
    }

    fn mark_dropped(&self, _index: usize) {
        self.mark_reason(REASON_RESPONSE_INCOMPLETE);
    }

    fn mark_transport_error(&self, _index: usize) {
        self.mark_reason(REASON_TRANSPORT_ERROR);
    }
}

fn add_reason(reasons: &mut Vec<String>, reason: &str) {
    if reasons.len() < MAX_REASONS && !reasons.iter().any(|existing| existing == reason) {
        reasons.push(reason.to_string());
    }
}

fn normalize_capture_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

struct Attempt {
    capture: CaptureHandle,
    index: usize,
}

impl Attempt {
    fn status(&self, status: StatusCode) {
        self.capture.set_status(self.index, status);
    }

    fn response_bytes(&self, amount: usize) {
        self.capture.add_response_bytes(self.index, amount);
    }

    fn response_complete(&self) {
        self.capture.mark_complete(self.index);
    }

    fn response_error(&self) {
        self.capture.mark_response_error(self.index);
    }

    fn dropped(&self) {
        self.capture.mark_dropped(self.index);
    }

    fn transport_error(&self) {
        self.capture.mark_transport_error(self.index);
    }
}

struct CaptureHttpFetch<F> {
    inner: F,
    capture: CaptureHandle,
}

impl<F> HttpFetch for CaptureHttpFetch<F>
where
    F: HttpFetch,
{
    async fn fetch(&self, req: Request<Buffer>) -> opendal::Result<Response<HttpBody>> {
        let parsed = self.capture.describe_request(&req);
        let attempt = self.capture.begin_attempt(parsed, req.body().len());
        let response = self.inner.fetch(req).await;
        let Some(attempt) = attempt else {
            return response;
        };
        match response {
            Err(error) => {
                attempt.transport_error();
                // Preserve the original OpenDAL error so its retry metadata
                // and temporary classification remain unchanged.
                Err(error)
            }
            Ok(response) => {
                let status = response.status();
                attempt.status(status);
                let (parts, body) = response.into_parts();
                let stream = CountingStream::new(body, attempt);
                Ok(Response::from_parts(parts, HttpBody::new(stream, None)))
            }
        }
    }
}

struct ReadState {
    reader: Arc<tokio::sync::Mutex<Box<dyn ReadDyn>>>,
    pending: Option<Pin<Box<dyn Future<Output = opendal::Result<Buffer>> + Send>>>,
    done: bool,
}

struct CountingStream {
    state: Arc<Mutex<ReadState>>,
    attempt: Option<Attempt>,
}

impl CountingStream {
    fn new(body: HttpBody, attempt: Attempt) -> Self {
        Self {
            state: Arc::new(Mutex::new(ReadState {
                reader: Arc::new(tokio::sync::Mutex::new(Box::new(body))),
                pending: None,
                done: false,
            })),
            attempt: Some(attempt),
        }
    }
}

impl Stream for CountingStream {
    type Item = opendal::Result<Buffer>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut state = this
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.done {
            return Poll::Ready(None);
        }
        if state.pending.is_none() {
            let reader = Arc::clone(&state.reader);
            state.pending = Some(Box::pin(async move {
                let mut reader = reader.lock().await;
                reader.read_dyn().await
            }));
        }
        let pending = state.pending.as_mut().expect("pending read installed");
        match pending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                state.pending = None;
                match result {
                    Ok(buffer) if buffer.is_empty() => {
                        state.done = true;
                        if let Some(attempt) = this.attempt.as_ref() {
                            attempt.response_complete();
                        }
                        Poll::Ready(None)
                    }
                    Ok(buffer) => {
                        if let Some(attempt) = this.attempt.as_ref() {
                            attempt.response_bytes(buffer.len());
                        }
                        Poll::Ready(Some(Ok(buffer)))
                    }
                    Err(error) => {
                        state.done = true;
                        if let Some(attempt) = this.attempt.as_ref() {
                            attempt.response_error();
                        }
                        Poll::Ready(Some(Err(error)))
                    }
                }
            }
        }
    }
}

impl Drop for CountingStream {
    fn drop(&mut self) {
        let done = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .done;
        if !done {
            if let Some(attempt) = self.attempt.as_ref() {
                attempt.dropped();
            }
        }
    }
}

#[derive(Debug)]
struct ParsedRequest {
    method: String,
    action: String,
    selector: Option<String>,
    foreign_selector: bool,
}

fn describe_request(req: &Request<Buffer>, bucket: &str, prefix: &str) -> ParsedRequest {
    let method = safe_method(req.method());
    let Some(method_kind) = method_kind(req.method()) else {
        return ParsedRequest {
            method,
            action: "unknown".to_string(),
            selector: None,
            foreign_selector: false,
        };
    };
    let Some(decoded_path) = decode_component(req.uri().path(), false) else {
        return ParsedRequest {
            method,
            action: "unknown".to_string(),
            selector: None,
            foreign_selector: false,
        };
    };
    let Some(path) = decoded_path.strip_prefix('/').map(str::to_string) else {
        return ParsedRequest {
            method,
            action: "unknown".to_string(),
            selector: None,
            foreign_selector: false,
        };
    };
    let host_bucket = req
        .headers()
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.split('.').next())
        .is_some_and(|candidate| candidate == bucket);
    let path_without_bucket = if path == bucket {
        Some("")
    } else if let Some(rest) = path.strip_prefix(&format!("{bucket}/")) {
        Some(rest)
    } else if host_bucket {
        Some(path.as_str())
    } else {
        None
    };
    let Some(path_without_bucket) = path_without_bucket else {
        return ParsedRequest {
            method,
            action: "unknown".to_string(),
            selector: None,
            foreign_selector: true,
        };
    };
    let query = parse_query(req.uri().query());
    if query.malformed || query.duplicate {
        return ParsedRequest {
            method,
            action: "unknown".to_string(),
            selector: None,
            foreign_selector: false,
        };
    }
    let object_key = path_without_bucket.to_string();
    let action = classify_action(
        method_kind,
        &query,
        path_without_bucket.is_empty(),
        req.headers().contains_key("x-amz-copy-source"),
    );
    let selector = if action == "ListObjectsV2" {
        query
            .pairs
            .get("prefix")
            .cloned()
            .unwrap_or_else(|| object_key.clone())
    } else {
        object_key
    };
    let decoded_prefix = prefix.trim_matches('/');
    let selector_is_in_scope = action == "HeadBucket"
        || decoded_prefix.is_empty()
        || selector == decoded_prefix
        || selector.starts_with(&format!("{decoded_prefix}/"));
    if !selector_is_in_scope {
        return ParsedRequest {
            method,
            action: "unknown".to_string(),
            selector: None,
            foreign_selector: true,
        };
    }
    if selector.len() > MAX_SELECTOR_BYTES {
        return ParsedRequest {
            method,
            action: "unknown".to_string(),
            selector: None,
            foreign_selector: false,
        };
    }
    let selector = if action == "unknown" {
        None
    } else {
        Some(selector)
    };
    ParsedRequest {
        method,
        action: action.to_string(),
        selector,
        foreign_selector: false,
    }
}

#[derive(Clone, Copy)]
enum MethodKind {
    Get,
    Head,
    Put,
    Post,
    Delete,
}

fn method_kind(method: &Method) -> Option<MethodKind> {
    match *method {
        Method::GET => Some(MethodKind::Get),
        Method::HEAD => Some(MethodKind::Head),
        Method::PUT => Some(MethodKind::Put),
        Method::POST => Some(MethodKind::Post),
        Method::DELETE => Some(MethodKind::Delete),
        _ => None,
    }
}

fn safe_method(method: &Method) -> String {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::PUT => "PUT",
        Method::POST => "POST",
        Method::DELETE => "DELETE",
        _ => "OTHER",
    }
    .to_string()
}

struct ParsedQuery {
    pairs: BTreeMap<String, String>,
    duplicate: bool,
    malformed: bool,
}

fn parse_query(query: Option<&str>) -> ParsedQuery {
    let mut parsed = ParsedQuery {
        pairs: BTreeMap::new(),
        duplicate: false,
        malformed: false,
    };
    let Some(query) = query else {
        return parsed;
    };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (Some(key), Some(value)) = (decode_component(key, true), decode_component(value, true))
        else {
            parsed.malformed = true;
            continue;
        };
        if parsed.pairs.insert(key, value).is_some() {
            parsed.duplicate = true;
        }
    }
    parsed
}

/// Strictly percent-decode a URI component. OpenDAL's helper intentionally
/// falls back to the original string on malformed UTF-8; capture must never
/// persist such an undecoded target, so malformed bytes are rejected.
fn decode_component(value: &str, plus_as_space: bool) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1])?;
                let low = hex_value(bytes[index + 2])?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            b'%' => return None,
            b'+' if plus_as_space => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn classify_action(
    method: MethodKind,
    query: &ParsedQuery,
    bucket_root: bool,
    copy_source: bool,
) -> &'static str {
    let pairs = &query.pairs;
    match method {
        MethodKind::Get => {
            if bucket_root && is_list_v2_query(pairs) {
                "ListObjectsV2"
            } else if !bucket_root
                && pairs.keys().all(|key| {
                    matches!(
                        key.as_str(),
                        "versionId"
                            | "response-content-disposition"
                            | "response-content-type"
                            | "response-cache-control"
                    )
                })
            {
                "GetObject"
            } else {
                "unknown"
            }
        }
        MethodKind::Head => {
            if bucket_root && pairs.is_empty() {
                "HeadBucket"
            } else if !bucket_root && pairs.keys().all(|key| key == "versionId") {
                "HeadObject"
            } else {
                "unknown"
            }
        }
        MethodKind::Put => {
            if !copy_source
                && !bucket_root
                && pairs.len() == 2
                && pairs.contains_key("partNumber")
                && pairs.contains_key("uploadId")
                && pairs
                    .get("partNumber")
                    .is_some_and(|value| value.parse::<u64>().is_ok_and(|number| number > 0))
                && pairs.get("uploadId").is_some_and(|value| !value.is_empty())
            {
                "UploadPart"
            } else if !copy_source && !bucket_root && pairs.is_empty() {
                "PutObject"
            } else {
                "unknown"
            }
        }
        MethodKind::Post => {
            if !bucket_root && pairs.len() == 1 && pairs.get("uploads") == Some(&String::new()) {
                "CreateMultipartUpload"
            } else if !bucket_root
                && pairs.len() == 1
                && pairs.get("uploadId").is_some_and(|value| !value.is_empty())
            {
                "CompleteMultipartUpload"
            } else {
                "unknown"
            }
        }
        MethodKind::Delete => {
            if !bucket_root
                && pairs.len() == 1
                && pairs.get("uploadId").is_some_and(|value| !value.is_empty())
            {
                "AbortMultipartUpload"
            } else if !bucket_root
                && (pairs.is_empty()
                    || (pairs.len() == 1
                        && pairs
                            .get("versionId")
                            .is_some_and(|value| !value.is_empty())))
            {
                "DeleteObject"
            } else {
                "unknown"
            }
        }
    }
}

fn is_list_v2_query(pairs: &BTreeMap<String, String>) -> bool {
    pairs.get("list-type").is_some_and(|value| value == "2")
        && pairs.keys().all(|key| {
            matches!(
                key.as_str(),
                "list-type"
                    | "prefix"
                    | "delimiter"
                    | "max-keys"
                    | "start-after"
                    | "continuation-token"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use opendal::raw::oio::Read as OioRead;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn request(method: Method, uri: &str, body: Buffer) -> Request<Buffer> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .unwrap()
    }

    #[test]
    fn classifies_safe_s3_actions_and_decodes_selectors() {
        let get = request(
            Method::GET,
            "https://r2.example/ws/repos%2Fbase%2Fconfig",
            Buffer::new(),
        );
        let parsed = describe_request(&get, "ws", "/repos/base/");
        assert_eq!(parsed.action, "GetObject");
        assert_eq!(parsed.selector.as_deref(), Some("repos/base/config"));

        let list = request(
            Method::GET,
            "https://r2.example/ws?list-type=2&prefix=repos%2Fbase%2F",
            Buffer::new(),
        );
        let parsed = describe_request(&list, "ws", "/repos/base/");
        assert_eq!(parsed.action, "ListObjectsV2");
        assert_eq!(parsed.selector.as_deref(), Some("repos/base/"));

        let head_bucket = request(Method::HEAD, "https://r2.example/ws", Buffer::new());
        let parsed = describe_request(&head_bucket, "ws", "/repos/base/");
        assert_eq!(parsed.action, "HeadBucket");
        assert_eq!(parsed.selector.as_deref(), Some(""));
    }

    #[test]
    fn foreign_or_unknown_selectors_never_leak_targets() {
        let foreign = request(
            Method::GET,
            "https://r2.example/other/private?token=secret",
            Buffer::new(),
        );
        let parsed = describe_request(&foreign, "ws", "/repos/base/");
        assert_eq!(parsed.action, "unknown");
        assert!(parsed.selector.is_none());
        assert!(parsed.foreign_selector);

        let long = "x".repeat(MAX_SELECTOR_BYTES + 1);
        let request = request(
            Method::GET,
            &format!("https://r2.example/ws/repos/base/{long}"),
            Buffer::new(),
        );
        let parsed = describe_request(&request, "ws", "/repos/base/");
        assert_eq!(parsed.action, "unknown");
        assert!(parsed.selector.is_none());
    }

    #[test]
    fn capture_records_sequence_and_redacts_unknown_requests() {
        let capture = CaptureHandle::new(CaptureOperation::SnapshotRestore, "ws", "repos/base");
        let parsed = capture.describe_request(&request(
            Method::GET,
            "https://r2.example/ws/repos/base/config?versionId=secret",
            Buffer::new(),
        ));
        let attempt = capture.begin_attempt(parsed, 5).unwrap();
        attempt.status(StatusCode::OK);
        attempt.response_bytes(3);
        attempt.response_complete();
        let document = capture.finish(true);
        assert_eq!(document.records[0].id, format!("{}:1", document.capture_id));
        assert_eq!(document.records[0].request_body_bytes, 5);
        assert_eq!(document.records[0].response_body_bytes, 3);
        assert_eq!(document.status, CaptureStatus::Complete);
        assert!(serde_json::to_string(&document)
            .unwrap()
            .contains("GetObject"));
        assert!(!serde_json::to_string(&document).unwrap().contains("secret"));
    }

    #[test]
    fn record_limit_is_bounded() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "");
        for _ in 0..(MAX_RECORDS + 4) {
            let parsed = capture.describe_request(&request(
                Method::HEAD,
                "https://r2.example/b",
                Buffer::new(),
            ));
            if let Some(attempt) = capture.begin_attempt(parsed, 0) {
                attempt.status(StatusCode::OK);
                attempt.response_complete();
            }
        }
        let document = capture.finish(true);
        assert_eq!(document.records.len(), MAX_RECORDS);
        assert_eq!(document.reasons, vec![REASON_RECORD_LIMIT.to_string()]);
        assert_eq!(document.status, CaptureStatus::Incomplete);
    }

    #[tokio::test]
    async fn counting_stream_marks_eof_and_drop() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "");
        let attempt = capture
            .begin_attempt(
                capture.describe_request(&request(
                    Method::GET,
                    "https://r2.example/b/key",
                    Buffer::new(),
                )),
                0,
            )
            .unwrap();
        let body = HttpBody::new(
            futures_util::stream::iter(vec![Ok(Buffer::from("abc")), Ok(Buffer::new())]),
            None,
        );
        let mut stream = CountingStream::new(body, attempt);
        assert_eq!(stream.next().await.unwrap().unwrap().to_vec(), b"abc");
        assert!(stream.next().await.is_none());
        let document = capture.finish(true);
        assert_eq!(document.records[0].response_body_bytes, 3);
        assert!(document.records[0].response_complete);

        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "");
        let attempt = capture
            .begin_attempt(
                capture.describe_request(&request(
                    Method::GET,
                    "https://r2.example/b/key",
                    Buffer::new(),
                )),
                0,
            )
            .unwrap();
        let body = HttpBody::new(
            futures_util::stream::iter(vec![Ok(Buffer::from("abc")), Ok(Buffer::new())]),
            None,
        );
        let mut stream = CountingStream::new(body, attempt);
        let _ = stream.next().await;
        drop(stream);
        let document = capture.finish(true);
        assert_eq!(document.records[0].response_body_bytes, 3);
        assert!(!document.records[0].response_complete);
        assert!(document
            .reasons
            .contains(&REASON_RESPONSE_INCOMPLETE.to_string()));
    }

    #[tokio::test]
    async fn inner_content_length_mismatch_is_incomplete() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "");
        let attempt = capture
            .begin_attempt(
                capture.describe_request(&request(
                    Method::GET,
                    "https://r2.example/b/key",
                    Buffer::new(),
                )),
                0,
            )
            .unwrap();
        let body = HttpBody::new(
            futures_util::stream::iter(vec![Ok(Buffer::from("abc"))]),
            Some(5),
        );
        let mut stream = CountingStream::new(body, attempt);
        assert_eq!(stream.next().await.unwrap().unwrap().to_vec(), b"abc");
        assert!(stream.next().await.unwrap().is_err());
        let document = capture.finish(true);
        assert_eq!(document.records[0].response_body_bytes, 3);
        assert!(!document.records[0].response_complete);
        assert!(document
            .reasons
            .contains(&REASON_RESPONSE_ERROR.to_string()));
    }

    #[derive(Default)]
    struct FakeHttpFetch {
        calls: AtomicUsize,
    }

    impl HttpFetch for FakeHttpFetch {
        async fn fetch(&self, _req: Request<Buffer>) -> opendal::Result<Response<HttpBody>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Err(opendal::Error::new(
                    opendal::ErrorKind::Unexpected,
                    "temporary test error",
                )
                .set_temporary());
            }
            let body = HttpBody::new(
                futures_util::stream::iter(vec![Ok(Buffer::from("ok")), Ok(Buffer::new())]),
                None,
            );
            Ok(Response::builder()
                .status(StatusCode::OK)
                .body(body)
                .unwrap())
        }
    }

    #[tokio::test]
    async fn fake_retry_attempts_are_recorded_individually() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "repo");
        let client = HttpClient::with(CaptureHttpFetch {
            inner: FakeHttpFetch::default(),
            capture: capture.clone(),
        });
        let make_request = || {
            request(
                Method::GET,
                "https://r2.example/b/repo/index",
                Buffer::new(),
            )
        };

        let first = client.fetch(make_request()).await;
        assert!(first.is_err());
        let mut response = client.fetch(make_request()).await.unwrap();
        while !response.body_mut().read().await.unwrap().is_empty() {}

        let document = capture.finish(true);
        assert_eq!(document.records.len(), 2);
        assert_eq!(document.records[0].action, "GetObject");
        assert_eq!(document.records[0].status, None);
        assert_eq!(document.records[1].status, Some(200));
        assert!(document.records[1].response_complete);
        assert!(document
            .reasons
            .contains(&REASON_TRANSPORT_ERROR.to_string()));
    }

    #[tokio::test]
    async fn list_pagination_records_each_safe_prefix() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "repo");
        let client = HttpClient::with(CaptureHttpFetch {
            inner: FakeHttpFetch {
                calls: AtomicUsize::new(1),
            },
            capture: capture.clone(),
        });
        for token in ["", "next%2Btoken"] {
            let uri = if token.is_empty() {
                "https://r2.example/b?list-type=2&prefix=repo%2F"
            } else {
                "https://r2.example/b?list-type=2&prefix=repo%2F&continuation-token=next%2Btoken"
            };
            let mut response = client
                .fetch(request(Method::GET, uri, Buffer::new()))
                .await
                .unwrap();
            while !response.body_mut().read().await.unwrap().is_empty() {}
        }
        let document = capture.finish(true);
        assert_eq!(document.records.len(), 2);
        assert!(document
            .records
            .iter()
            .all(|record| record.action == "ListObjectsV2"));
        assert!(document
            .records
            .iter()
            .all(|record| record.selector.as_deref() == Some("repo/")));
    }

    struct StatusSequence {
        calls: AtomicUsize,
    }

    impl HttpFetch for StatusSequence {
        async fn fetch(&self, _req: Request<Buffer>) -> opendal::Result<Response<HttpBody>> {
            let status = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            };
            let body = HttpBody::new(futures_util::stream::iter(vec![Ok(Buffer::new())]), None);
            Ok(Response::builder().status(status).body(body).unwrap())
        }
    }

    #[tokio::test]
    async fn opendal_retry_layer_reaches_capture_for_each_attempt() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "repo");
        let client = HttpClient::with(CaptureHttpFetch {
            inner: StatusSequence {
                calls: AtomicUsize::new(0),
            },
            capture: capture.clone(),
        });
        let options = [
            ("endpoint".to_string(), "http://127.0.0.1:9".to_string()),
            ("region".to_string(), "auto".to_string()),
            ("bucket".to_string(), "b".to_string()),
            ("root".to_string(), "/repo/".to_string()),
            ("access_key_id".to_string(), "access".to_string()),
            ("secret_access_key".to_string(), "secret".to_string()),
            ("disable_config_load".to_string(), "true".to_string()),
            ("disable_ec2_metadata".to_string(), "true".to_string()),
        ];
        let operator = opendal::Operator::via_iter("s3", options)
            .unwrap()
            .layer(opendal::layers::HttpClientLayer::new(client))
            .layer(opendal::layers::RetryLayer::new().with_max_times(1));
        assert!(operator.stat("object").await.is_ok());

        let document = capture.finish(true);
        assert_eq!(document.records.len(), 2);
        assert_eq!(document.records[0].status, Some(503));
        assert_eq!(document.records[1].status, Some(200));
        assert!(document
            .reasons
            .contains(&REASON_RESPONSE_ERROR.to_string()));
    }

    #[test]
    fn malformed_percent_encoding_never_becomes_a_selector() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "repo");
        let parsed = capture.describe_request(&request(
            Method::GET,
            "https://r2.example/b/repo/%FF",
            Buffer::new(),
        ));
        assert_eq!(parsed.action, "unknown");
        assert!(parsed.selector.is_none());
    }

    #[test]
    fn list_query_on_an_object_path_is_unknown() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "repo");
        let parsed = capture.describe_request(&request(
            Method::GET,
            "https://r2.example/b/repo/object?list-type=2&prefix=repo%2F",
            Buffer::new(),
        ));
        assert_eq!(parsed.action, "unknown");
        assert!(parsed.selector.is_none());
    }

    #[test]
    fn multipart_and_copy_requests_use_closed_action_sets() {
        let capture = CaptureHandle::new(CaptureOperation::Verification, "b", "repo");
        let upload = capture.describe_request(&request(
            Method::PUT,
            "https://r2.example/b/repo/object?partNumber=1&uploadId=u",
            Buffer::from("part"),
        ));
        assert_eq!(upload.action, "UploadPart");
        let unknown = capture.describe_request(&request(
            Method::PUT,
            "https://r2.example/b/repo/object?partNumber=1&uploadId=u&extra=x",
            Buffer::new(),
        ));
        assert_eq!(unknown.action, "unknown");
        let copy = Request::builder()
            .method(Method::PUT)
            .uri("https://r2.example/b/repo/object")
            .header("x-amz-copy-source", "b/repo/other")
            .body(Buffer::new())
            .unwrap();
        assert_eq!(capture.describe_request(&copy).action, "unknown");
    }
}
