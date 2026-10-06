//! `SandboxHttp` over a libkrun microVM — a small HTTP/1.1 client that reaches
//! the in-guest `opencode serve` through a vsock socket.
//!
//! The guest runs `pillbox vsock-forward` ([`attach::host::run_vsock_forward`]),
//! which listens on a vsock port and bridges each connection to
//! `127.0.0.1:<opencode port>`. libkrun binds the host side of that vsock port
//! at `host_sock` (`krun_add_vsock_port2` listen=true — the same guest-listens
//! mechanism `--detach` uses). So each call here is: connect `host_sock` → speak
//! HTTP → read the response. One connection per call, so concurrent readiness
//! polls are independent vsock streams.
//!
//! Why hand-rolled and not a crate: three trivial calls (`GET /api/info`, `POST
//! /api/session`, `POST …/prompt`) over a unix socket — pulling
//! in an HTTP-client dep (with its own connector model) to reach a socket we
//! already hold would be heavier than the ~30 lines here.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::sandbox::http::{HttpResponse, SandboxHttp};

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
                "connecting to the guest opencode forward at {}",
                self.host_sock.display()
            )
        })
    }
}

/// Build a one-shot HTTP/1.1 request head with `Connection: close`, so the
/// server closes after the body and `read_to_end` terminates.
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

/// Parse a full HTTP/1.1 response (status line + headers + body, headers
/// terminated by CRLFCRLF). Returns (status, body verbatim — no de-chunking;
/// opencode's one-shot replies are small `Content-Length` bodies).
fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("opencode response: no header terminator")?;
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
        .with_context(|| format!("opencode response: bad status line: {status_line:?}"))?;
    let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
    Ok((status, body))
}

impl SandboxHttp for LibkrunHttp {
    fn request(&self, method: &str, path: &str, json_body: Option<&str>) -> Result<HttpResponse> {
        let mut s = self.connect()?;
        s.write_all(request_head(method, path, json_body, &self.auth_header()).as_bytes())?;
        if let Some(b) = json_body {
            s.write_all(b.as_bytes())?;
        }
        s.flush()?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?; // Connection: close → server closes after body
        let (status, body) = parse_response(&raw)?;
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

    #[test]
    fn missing_terminator_is_error() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nno end").is_err());
    }
}
