//! `SandboxHttp` over a libkrun microVM — a small HTTP/1.1 client that reaches
//! a server agent's in-guest HTTP server (`opencode serve`, or the codex
//! app-server bridge) through a vsock socket.
//!
//! The guest runs `pillbox vsock-forward` ([`attach::host::run_vsock_forward`]),
//! which listens on a vsock port and bridges each connection to
//! `127.0.0.1:<server port>`. libkrun binds the host side of that vsock port
//! at `host_sock` (`krun_add_vsock_port2` listen=true — the same guest-listens
//! mechanism `--detach` uses). So each call here is: connect `host_sock` → speak
//! HTTP → read the response. One connection per call, so concurrent readiness
//! polls are independent vsock streams.
//!
//! Why hand-rolled and not a crate: a few trivial one-shot calls (`GET /api/info`,
//! `POST /api/session`, `POST …/prompt`) over a unix socket — pulling
//! in an HTTP-client dep (with its own connector model) to reach a socket we
//! already hold would be heavier than the ~30 lines here.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::sandbox::http::{HttpResponse, SandboxHttp};

/// Most bytes one guest reply may carry, head included. The replies read here are
/// small JSON (`/api/info`, a session id, a prompt or turn echo that can repeat a long
/// prompt), so 16 MiB is orders of magnitude of headroom, not a tight fit.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Wall-clock bound on one request/response exchange. Every call here returns as
/// soon as the guest server admits the request (it never waits on a model turn),
/// so a minute only ever trips on a guest server that has stopped answering.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct LibkrunHttp {
    host_sock: PathBuf,
    /// A ready `Authorization` header value, sent on every request when set
    /// (OpenCode 2's server always requires HTTP basic auth).
    authorization: Option<String>,
}

impl LibkrunHttp {
    pub(crate) fn new(host_sock: PathBuf) -> Self {
        Self {
            host_sock,
            authorization: None,
        }
    }

    /// The same forward, authenticating every request with HTTP basic auth.
    pub(crate) fn with_basic_auth(&self, user: &str, password: &str) -> Self {
        use base64::Engine as _;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        Self {
            host_sock: self.host_sock.clone(),
            authorization: Some(format!("Basic {token}")),
        }
    }

    fn auth_header(&self) -> String {
        self.authorization
            .as_deref()
            .map(|value| format!("Authorization: {value}\r\n"))
            .unwrap_or_default()
    }

    fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.host_sock).with_context(|| {
            format!(
                "connecting to the guest server forward at {}",
                self.host_sock.display()
            )
        })
    }
}

/// Build a one-shot HTTP/1.1 request head with `Connection: close`, so a server
/// that omits `Content-Length` still ends the body by closing.
fn request_head(method: &str, path: &str, json_body: Option<&str>, auth: &str) -> String {
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}");
    if let Some(b) = json_body {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    req
}

/// Byte offset just past the `\r\n\r\n` that ends the head, once it has arrived.
fn head_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// The `Content-Length` a head declares, if any.
fn content_length(head: &[u8]) -> Result<Option<usize>> {
    for line in String::from_utf8_lossy(head).split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                let len = value
                    .trim()
                    .parse()
                    .with_context(|| format!("guest server reply: bad Content-Length {value:?}"))?;
                return Ok(Some(len));
            }
        }
    }
    Ok(None)
}

/// Send `request` on `stream` and read the reply. The guest server on the other end is
/// untrusted: the reply is bounded by `MAX_RESPONSE_BYTES` and the whole exchange by
/// `timeout`, so a server that streams forever or never closes errors instead of
/// exhausting or wedging the host.
fn exchange(mut stream: UnixStream, request: &[u8], timeout: Duration) -> Result<(u16, String)> {
    let deadline = Instant::now() + timeout;
    let timed_out = || anyhow::anyhow!("guest server reply: no complete reply within {timeout:?}");
    // Writes share the read deadline: each write gets only the time left, so a guest that
    // never reads cannot hold the request past `timeout`.
    let mut sent = 0;
    while sent < request.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(&request[sent..]) {
            Ok(0) => bail!("guest server reply: connection closed while sending the request"),
            Ok(n) => sent += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(timed_out())
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mut raw = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    // Set once the head has arrived and declared a Content-Length: total bytes to read.
    let mut want: Option<usize> = None;
    loop {
        if want.is_none() {
            if let Some(end) = head_end(&raw) {
                if let Some(len) = content_length(&raw[..end])? {
                    if len > MAX_RESPONSE_BYTES - end {
                        bail!("guest server reply: Content-Length {len} exceeds the {MAX_RESPONSE_BYTES}-byte cap");
                    }
                    want = Some(end + len);
                }
            }
        }
        if let Some(want) = want {
            if raw.len() >= want {
                raw.truncate(want);
                break;
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        stream.set_read_timeout(Some(remaining))?;
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(timed_out())
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            // Connection: close, no Content-Length: EOF ends the body.
            if want.is_some() {
                bail!("guest server reply: connection closed before Content-Length bytes arrived");
            }
            break;
        }
        if raw.len() + n > MAX_RESPONSE_BYTES {
            bail!("guest server reply: exceeds the {MAX_RESPONSE_BYTES}-byte cap");
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    parse_response(&raw)
}

/// Parse a full HTTP/1.1 response (status line + headers + body, headers
/// terminated by CRLFCRLF). Returns (status, body verbatim — no de-chunking;
/// the guest servers' one-shot replies are small `Content-Length` bodies).
fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("guest server reply: no header terminator")?;
    let status_line = raw[..split]
        .split(|&b| b == b'\n')
        .next()
        .unwrap_or(&raw[..split]);
    let status_line = String::from_utf8_lossy(status_line);
    // "HTTP/1.1 200 OK" → 200
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("guest server reply: bad status line: {status_line:?}"))?;
    let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
    Ok((status, body))
}

impl SandboxHttp for LibkrunHttp {
    fn request(&self, method: &str, path: &str, json_body: Option<&str>) -> Result<HttpResponse> {
        let mut request = request_head(method, path, json_body, &self.auth_header()).into_bytes();
        request.extend_from_slice(json_body.unwrap_or_default().as_bytes());
        let (status, body) = exchange(self.connect()?, &request, RESPONSE_TIMEOUT)?;
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"id\":\"ses_1\"}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"id\":\"ses_1\"}");
    }

    #[test]
    fn parses_empty_body_204() {
        let raw = b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 204);
        assert_eq!(body, "");
    }

    #[test]
    fn basic_auth_header_rides_every_request() {
        let http =
            LibkrunHttp::new(PathBuf::from("/nonexistent")).with_basic_auth("opencode", "pw");
        let head = request_head("GET", "/api/info", None, &http.auth_header());
        // base64("opencode:pw") == "b3BlbmNvZGU6cHc="
        assert!(
            head.contains("Authorization: Basic b3BlbmNvZGU6cHc=\r\n"),
            "{head}"
        );
        assert!(head.ends_with("\r\n\r\n"));
        let plain = LibkrunHttp::new(PathBuf::from("/nonexistent"));
        assert!(!request_head("GET", "/x", None, &plain.auth_header()).contains("Authorization"));
    }

    /// A fake guest server on the far end of a socket pair: drains the request
    /// head, then runs `reply` on its end of the stream.
    fn guest(reply: impl FnOnce(&mut UnixStream) + Send + 'static) -> UnixStream {
        let (host, mut peer) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while head_end(&seen).is_none() && peer.read(&mut byte).unwrap_or(0) == 1 {
                seen.push(byte[0]);
            }
            reply(&mut peer);
        });
        host
    }

    #[test]
    fn a_normal_reply_parses_over_the_socket() {
        let host = guest(|peer| {
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
            // Holding the connection open past the body must not matter.
            std::thread::sleep(Duration::from_secs(2));
        });
        let req = request_head("GET", "/api/info", None, "");
        let (status, body) = exchange(host, req.as_bytes(), Duration::from_secs(1)).unwrap();
        assert_eq!((status, body.as_str()), (200, "{}"));
    }

    #[test]
    fn a_reply_without_content_length_ends_at_close() {
        let host = guest(|peer| {
            peer.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        });
        let (status, body) = exchange(host, b"GET / HTTP/1.1\r\n\r\n", RESPONSE_TIMEOUT).unwrap();
        assert_eq!((status, body.as_str()), (204, ""));
    }

    #[test]
    fn an_endless_body_errors_at_the_cap() {
        let host = guest(|peer| {
            let _ = peer.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            let junk = [b'x'; 64 * 1024];
            while peer.write_all(&junk).is_ok() {}
        });
        let err = exchange(host, b"GET / HTTP/1.1\r\n\r\n", RESPONSE_TIMEOUT).unwrap_err();
        assert!(err.to_string().contains("cap"), "{err:#}");
    }

    #[test]
    fn an_oversized_content_length_errors_before_reading_the_body() {
        let host = guest(|peer| {
            let _ = peer.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    MAX_RESPONSE_BYTES
                )
                .as_bytes(),
            );
            std::thread::sleep(Duration::from_secs(2));
        });
        let err = exchange(host, b"GET / HTTP/1.1\r\n\r\n", Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("cap"), "{err:#}");
    }

    #[test]
    fn a_silent_guest_errors_at_the_timeout() {
        let host = guest(|peer| {
            let _ = peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{");
            std::thread::sleep(Duration::from_secs(5));
        });
        let started = Instant::now();
        let err =
            exchange(host, b"GET / HTTP/1.1\r\n\r\n", Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("within"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_guest_that_never_reads_errors_at_the_deadline() {
        let (host, peer) = UnixStream::pair().unwrap();
        // The peer holds its end open and never reads, so the socket buffer fills.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            drop(peer);
        });
        let mut request = request_head("POST", "/x", Some(""), "").into_bytes();
        request.resize(request.len() + 64 * 1024 * 1024, b'x');
        let timeout = Duration::from_millis(300);
        let started = Instant::now();
        let err = exchange(host, &request, timeout).unwrap_err();
        let elapsed = started.elapsed();
        assert!(err.to_string().contains("within"), "{err:#}");
        assert!(elapsed >= timeout / 2, "{elapsed:?}");
        assert!(elapsed < timeout + Duration::from_secs(2), "{elapsed:?}");
    }

    /// Each write makes progress here, so only a whole-request deadline bounds it.
    #[test]
    fn a_guest_that_reads_a_trickle_errors_at_the_deadline() {
        let (host, mut peer) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut sip = [0u8; 4096];
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(5) && peer.read(&mut sip).is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let mut request = request_head("POST", "/x", Some(""), "").into_bytes();
        request.resize(request.len() + 64 * 1024 * 1024, b'x');
        let timeout = Duration::from_millis(300);
        let started = Instant::now();
        let err = exchange(host, &request, timeout).unwrap_err();
        assert!(err.to_string().contains("within"), "{err:#}");
        assert!(started.elapsed() < timeout + Duration::from_secs(2));
    }

    #[test]
    fn missing_terminator_is_error() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nno end").is_err());
    }
}
