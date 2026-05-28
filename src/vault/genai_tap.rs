//! Streaming body tap that extracts Anthropic gen_ai usage from an
//! SSE response without breaking the downstream stream.
//!
//! [`TappedBody`] wraps a [`hudsucker::Body`] and forwards every
//! frame unchanged to whatever's consuming the response (Claude
//! Code, the user's terminal, etc.). On the way past, Data frames
//! are fed into an [`AnthropicSseParser`] that accumulates the
//! response's `message_start.message.{model,id,usage}` and
//! `message_delta.{delta.stop_reason,usage.output_tokens}`. When
//! the wrapped body ends — either via normal end-of-stream or
//! because the consumer dropped it — a single `on_end` callback
//! fires with the accumulated [`GenAiUsage`]. Callers attach span
//! emission to that callback.
//!
//! The parser is robust to non-SSE bodies (it simply produces an
//! empty [`GenAiUsage`]) and to chunk boundaries mid-line (raw
//! bytes accumulate until a `\n` lands).

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use hudsucker::{
    hyper::body::{Body as HttpBody, Bytes, Frame, SizeHint},
    Body, Error,
};

use crate::events::GenAiUsage;

/// Body wrapper that taps frames as they pass through. See module
/// doc for the contract.
pub(crate) struct TappedBody {
    inner: Body,
    parser: AnthropicSseParser,
    on_end: Option<Box<dyn FnOnce(GenAiUsage) + Send + Sync + 'static>>,
}

impl TappedBody {
    pub(crate) fn new(
        inner: Body,
        on_end: impl FnOnce(GenAiUsage) + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            parser: AnthropicSseParser::default(),
            on_end: Some(Box::new(on_end)),
        }
    }

    /// Fire the end callback exactly once, with whatever the parser
    /// has accumulated so far. Called on both natural end-of-stream
    /// and on drop (so a canceled stream still produces telemetry
    /// for the bytes that did make it through).
    ///
    /// **Off-runtime**: the callback runs on a fresh `std::thread`
    /// rather than inline. The OTel exporter uses
    /// `reqwest-blocking-client`, which creates a nested tokio
    /// runtime inside the blocking call. If we fired `on_end` inline,
    /// that nested runtime would be created *and dropped* on a tokio
    /// worker thread — Tokio panics on "Cannot drop a runtime in a
    /// context where blocking is not allowed." Spawning a real OS
    /// thread isolates the blocking export from the proxy's async
    /// context. Cost: one short-lived thread per intercepted call.
    fn fire_end(&mut self) {
        if let Some(on_end) = self.on_end.take() {
            let usage = std::mem::take(&mut self.parser).into_usage();
            std::thread::spawn(move || on_end(usage));
        }
    }
}

impl HttpBody for TappedBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        let this = self.as_mut().get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(None) => this.fire_end(),
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.parser.feed(data);
                }
            }
            _ => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TappedBody {
    fn drop(&mut self) {
        // Consumer dropped the body without polling to completion —
        // still fire so a canceled call shows up in telemetry rather
        // than vanishing entirely.
        self.fire_end();
    }
}

// ── SSE parser ─────────────────────────────────────────────────────

/// Line-buffered SSE parser tuned for Anthropic's `/v1/messages`
/// streaming response shape. Robust to partial-line chunk boundaries
/// (state carries via `buf`) and to interleaved non-data SSE lines
/// (`event:`, `id:`, `retry:` are skipped; only `data:` payloads
/// drive event dispatch).
#[derive(Default)]
struct AnthropicSseParser {
    /// Bytes seen since the last `\n`. Cleared as complete lines are
    /// drained.
    buf: Vec<u8>,
    /// `data:` payload accumulated for the current event. Per SSE
    /// spec, multiple `data:` lines join with `\n`; Anthropic uses
    /// one per event but we handle the general case.
    current_data: String,
    /// Full raw body, accumulated so [`Self::into_usage`] can fall
    /// back to a one-shot JSON parse when the response wasn't SSE
    /// (`stream: false` POSTs to `/v1/messages`). Cleared on the
    /// first successful SSE event so streaming responses don't pay
    /// the memory cost.
    raw_body: Vec<u8>,
    usage: GenAiUsage,
}

impl AnthropicSseParser {
    /// Feed a chunk of body bytes. Safe to call with partial lines —
    /// `buf` accumulates until a newline lands.
    fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        // Mirror into raw_body for the non-streaming JSON fallback.
        // First successful SSE event clears this — so SSE responses
        // only hold this buffer until message_start lands (a few
        // hundred bytes), not for the lifetime of the response.
        self.raw_body.extend_from_slice(chunk);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line_owned: Vec<u8> = self.buf.drain(..=nl).collect();
            let mut line: &[u8] = &line_owned;
            if let Some(stripped) = line.strip_suffix(b"\n") {
                line = stripped;
            }
            if let Some(stripped) = line.strip_suffix(b"\r") {
                line = stripped;
            }
            self.handle_line(line);
        }
    }

    fn handle_line(&mut self, line: &[u8]) {
        if line.is_empty() {
            // SSE event boundary.
            if !self.current_data.is_empty() {
                let payload = std::mem::take(&mut self.current_data);
                self.dispatch_event(&payload);
            }
            return;
        }
        let Some(rest) = line.strip_prefix(b"data:") else {
            return;
        };
        let Ok(text) = std::str::from_utf8(rest) else {
            return;
        };
        let text = text.strip_prefix(' ').unwrap_or(text);
        if !self.current_data.is_empty() {
            self.current_data.push('\n');
        }
        self.current_data.push_str(text);
    }

    fn dispatch_event(&mut self, data: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };
        match value.get("type").and_then(|v| v.as_str()) {
            Some("message_start") => {
                self.handle_message_start(&value);
                // Once we know it's SSE, drop the raw-body buffer —
                // the JSON fallback would never run anyway, and
                // long-context responses can be hundreds of KB.
                self.raw_body = Vec::new();
            }
            Some("message_delta") => self.handle_message_delta(&value),
            _ => {}
        }
    }

    fn handle_message_start(&mut self, v: &serde_json::Value) {
        let Some(msg) = v.get("message") else {
            return;
        };
        if let Some(s) = msg.get("model").and_then(|v| v.as_str()) {
            self.usage.response_model = Some(s.to_string());
        }
        if let Some(s) = msg.get("id").and_then(|v| v.as_str()) {
            self.usage.response_id = Some(s.to_string());
        }
        let Some(usage) = msg.get("usage") else {
            return;
        };
        // message_start carries the prompt-side numbers + an
        // initial output_tokens=1 placeholder. We ignore the
        // placeholder and let message_delta supply the final value.
        if let Some(n) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
            self.usage.input_tokens = Some(n);
        }
        if let Some(n) = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.usage.cache_read_input_tokens = Some(n);
        }
        if let Some(n) = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.usage.cache_creation_input_tokens = Some(n);
        }
    }

    fn handle_message_delta(&mut self, v: &serde_json::Value) {
        if let Some(s) = v.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
            self.usage.finish_reason = Some(s.to_string());
        }
        if let Some(n) = v.pointer("/usage/output_tokens").and_then(|v| v.as_u64()) {
            self.usage.output_tokens = Some(n);
        }
    }

    fn into_usage(mut self) -> GenAiUsage {
        // If no SSE events landed (non-streaming response, or an
        // error response with a JSON body), try parsing the
        // accumulated body as a single Anthropic Messages response.
        // Same usage shape as the SSE message_start / message_delta
        // events, just collapsed.
        if !self.raw_body.is_empty() {
            try_json_fallback(&self.raw_body, &mut self.usage);
        }
        self.usage
    }
}

/// Parse a non-streaming Anthropic `/v1/messages` response body and
/// fold its `model` / `id` / `usage` / `stop_reason` into `usage`.
/// Best-effort: non-JSON, error-shaped JSON, and partial JSON all
/// quietly leave `usage` as-is.
fn try_json_fallback(body: &[u8], usage: &mut GenAiUsage) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    if let Some(s) = v.get("model").and_then(|v| v.as_str()) {
        usage.response_model = Some(s.to_string());
    }
    if let Some(s) = v.get("id").and_then(|v| v.as_str()) {
        usage.response_id = Some(s.to_string());
    }
    if let Some(s) = v.get("stop_reason").and_then(|v| v.as_str()) {
        usage.finish_reason = Some(s.to_string());
    }
    let Some(u) = v.get("usage") else {
        return;
    };
    if let Some(n) = u.get("input_tokens").and_then(|v| v.as_u64()) {
        usage.input_tokens = Some(n);
    }
    if let Some(n) = u.get("output_tokens").and_then(|v| v.as_u64()) {
        usage.output_tokens = Some(n);
    }
    if let Some(n) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
        usage.cache_read_input_tokens = Some(n);
    }
    if let Some(n) = u
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
    {
        usage.cache_creation_input_tokens = Some(n);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;

    use http_body_util::BodyExt;

    use super::*;

    fn sample_sse() -> &'static [u8] {
        // Two events: message_start with prompt-side usage, then
        // message_delta with the final output_tokens + stop_reason.
        // Trailing blank line is the event boundary for message_delta.
        b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_abc\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":1247,\"cache_read_input_tokens\":8431,\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":89}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n"
    }

    #[test]
    fn parser_extracts_usage_from_full_anthropic_stream() {
        let mut p = AnthropicSseParser::default();
        p.feed(sample_sse());
        let u = p.into_usage();
        assert_eq!(u.response_model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(u.response_id.as_deref(), Some("msg_abc"));
        assert_eq!(u.input_tokens, Some(1247));
        assert_eq!(u.output_tokens, Some(89));
        assert_eq!(u.cache_read_input_tokens, Some(8431));
        assert_eq!(u.cache_creation_input_tokens, Some(0));
        assert_eq!(u.finish_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn parser_handles_chunk_boundaries_inside_lines() {
        // Split mid-token, mid-payload, mid-line — the parser must
        // wait for `\n` before processing.
        let full = sample_sse();
        let cuts = [3usize, 25, 73, 150, 200, 280, 400, full.len()];
        let mut p = AnthropicSseParser::default();
        let mut start = 0;
        for &cut in &cuts {
            let cut = cut.min(full.len());
            p.feed(&full[start..cut]);
            start = cut;
        }
        let u = p.into_usage();
        assert_eq!(u.input_tokens, Some(1247));
        assert_eq!(u.output_tokens, Some(89));
        assert_eq!(u.cache_read_input_tokens, Some(8431));
    }

    #[test]
    fn parser_ignores_non_sse_input() {
        // Random JSON body without SSE framing AND without an
        // Anthropic-shaped envelope produces no usage.
        let mut p = AnthropicSseParser::default();
        p.feed(b"{\"some\":\"object\"}\n");
        let u = p.into_usage();
        assert!(u.input_tokens.is_none());
        assert!(u.response_model.is_none());
    }

    #[test]
    fn parser_falls_back_to_json_for_non_streaming_response() {
        // `stream: false` POST /v1/messages returns a single JSON
        // body (not SSE). The fallback parses the same `model` +
        // `id` + `usage` + `stop_reason` fields the streaming
        // events would otherwise emit.
        let body = br#"{"id":"msg_xyz","model":"claude-opus-4-7","stop_reason":"end_turn","usage":{"input_tokens":500,"output_tokens":42,"cache_read_input_tokens":1024,"cache_creation_input_tokens":0}}"#;
        let mut p = AnthropicSseParser::default();
        p.feed(body);
        let u = p.into_usage();
        assert_eq!(u.response_model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(u.response_id.as_deref(), Some("msg_xyz"));
        assert_eq!(u.input_tokens, Some(500));
        assert_eq!(u.output_tokens, Some(42));
        assert_eq!(u.cache_read_input_tokens, Some(1024));
        assert_eq!(u.finish_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn parser_falls_back_with_chunked_json_body() {
        // The fallback must work even when the JSON arrives across
        // multiple chunks (real-world: large prompt responses
        // exceeding the TCP MSS).
        let body = br#"{"id":"msg_xyz","model":"claude-opus-4-7","usage":{"input_tokens":99}}"#;
        let mut p = AnthropicSseParser::default();
        // Feed in three chunks.
        let third = body.len() / 3;
        p.feed(&body[..third]);
        p.feed(&body[third..2 * third]);
        p.feed(&body[2 * third..]);
        let u = p.into_usage();
        assert_eq!(u.response_model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(u.input_tokens, Some(99));
    }

    #[test]
    fn sse_path_does_not_trigger_json_fallback() {
        // Sanity check: when SSE successfully parses, the raw_body
        // buffer gets cleared so the JSON fallback can't see any
        // stale bytes and accidentally overwrite a streaming value.
        let mut p = AnthropicSseParser::default();
        p.feed(sample_sse());
        let u = p.into_usage();
        // Output tokens came from message_delta (89), not from any
        // accidental JSON re-parse — pin it.
        assert_eq!(u.output_tokens, Some(89));
    }

    #[test]
    fn parser_handles_unknown_event_types_without_breaking() {
        let mut p = AnthropicSseParser::default();
        p.feed(b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        p.feed(sample_sse());
        let u = p.into_usage();
        // Final state matches the canonical stream — the ping event
        // didn't poison the parser.
        assert_eq!(u.input_tokens, Some(1247));
        assert_eq!(u.output_tokens, Some(89));
    }

    #[test]
    fn parser_handles_crlf_line_endings() {
        // Some proxies normalize SSE to CRLF; the parser strips both.
        let crlf = sample_sse()
            .iter()
            .copied()
            .flat_map(|b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect::<Vec<_>>();
        let mut p = AnthropicSseParser::default();
        p.feed(&crlf);
        let u = p.into_usage();
        assert_eq!(u.input_tokens, Some(1247));
        assert_eq!(u.output_tokens, Some(89));
    }

    /// Receive the on_end usage from the spawned worker thread.
    /// The callback runs off-runtime (see `TappedBody::fire_end`),
    /// so tests use a channel + bounded recv_timeout to wait for
    /// it rather than racing on a Mutex<Option<_>>.
    fn wait_for_usage(rx: &mpsc::Receiver<GenAiUsage>) -> GenAiUsage {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("on_end fired within 2s")
    }

    #[tokio::test]
    async fn tapped_body_forwards_bytes_and_fires_end_callback() {
        let inner = Body::from(sample_sse().to_vec());
        let (tx, rx) = mpsc::channel();
        let tapped = TappedBody::new(inner, move |usage| {
            let _ = tx.send(usage);
        });
        let collected = tapped.collect().await.expect("collect tapped body");
        let bytes = collected.to_bytes();
        // Bytes pass through unchanged.
        assert_eq!(&bytes[..], sample_sse());
        // End callback fired with the parsed usage.
        let u = wait_for_usage(&rx);
        assert_eq!(u.response_model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(u.input_tokens, Some(1247));
        assert_eq!(u.output_tokens, Some(89));
    }

    #[tokio::test]
    async fn tapped_body_fires_callback_when_dropped_mid_stream() {
        // Construct then immediately drop without polling — the
        // Drop impl should still fire so we don't silently lose
        // telemetry on canceled streams.
        let inner = Body::from(sample_sse().to_vec());
        let (tx, rx) = mpsc::channel();
        let tapped = TappedBody::new(inner, move |usage| {
            let _ = tx.send(usage);
        });
        drop(tapped);
        let u = wait_for_usage(&rx);
        // No bytes were parsed, so usage is empty — but the callback
        // still ran, which is the contract.
        assert!(u.input_tokens.is_none());
    }

    #[tokio::test]
    async fn tapped_body_does_not_drop_runtime_from_async_context() {
        // Regression guard for the "Cannot drop a runtime in a
        // context where blocking is not allowed" panic. The on_end
        // callback simulates the real-world OTel exporter (which
        // creates+drops a nested runtime via reqwest::blocking).
        // Done inline this would panic on the tokio worker; the
        // std::thread::spawn in fire_end isolates it.
        let inner = Body::from(sample_sse().to_vec());
        let (tx, rx) = mpsc::channel();
        let tapped = TappedBody::new(inner, move |usage| {
            // Build a tiny tokio runtime and drop it — mirrors what
            // reqwest::blocking does under the hood. Must succeed
            // because we're on a fresh OS thread, not a worker.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build inner runtime");
            drop(rt);
            let _ = tx.send(usage);
        });
        let _ = tapped.collect().await.expect("collect");
        let u = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(u) => u,
            Err(RecvTimeoutError::Timeout) => panic!("on_end did not complete"),
            Err(e) => panic!("recv: {e}"),
        };
        assert_eq!(u.input_tokens, Some(1247));
    }
}
