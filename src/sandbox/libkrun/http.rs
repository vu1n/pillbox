//! `SandboxHttp` over a libkrun microVM — a small HTTP/1.1 client that reaches
//! the in-guest `opencode serve` through a vsock socket.
//!
//! The guest runs `pillbox vsock-forward` ([`attach::host::run_vsock_forward`]),
//! which listens on a vsock port and bridges each connection to
//! `127.0.0.1:<opencode port>`. libkrun binds the host side of that vsock port
//! at `host_sock` (`krun_add_vsock_port2` listen=true — the same guest-listens
//! mechanism `--detach` uses). So each call here is: connect `host_sock` → speak
//! HTTP → read the response. One connection per call, so concurrent readiness
//! polls and the long-lived `/event` stream are independent vsock streams.
//!
//! Why hand-rolled and not a crate: four trivial calls (`GET /doc`, `POST
//! /session`, `POST /prompt_async`, `GET /event`) over a unix socket — pulling
//! in an HTTP-client dep (with its own connector model) to reach a socket we
//! already hold would be heavier than the ~30 lines here.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::sandbox::http::{HttpResponse, SandboxHttp, SandboxStream};

pub(crate) struct LibkrunHttp {
    host_sock: PathBuf,
}

impl LibkrunHttp {
    pub(crate) fn new(host_sock: PathBuf) -> Self {
        Self { host_sock }
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
/// server closes after the body and `read_to_end` terminates. (The SSE stream
/// builds its own keep-alive request inline in `open_stream`.)
fn request_head(method: &str, path: &str, json_body: Option<&str>) -> String {
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
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
        s.write_all(request_head(method, path, json_body).as_bytes())?;
        if let Some(b) = json_body {
            s.write_all(b.as_bytes())?;
        }
        s.flush()?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?; // Connection: close → server closes after body
        let (status, body) = parse_response(&raw)?;
        Ok(HttpResponse { status, body })
    }

    fn open_stream(&self, path: &str) -> Result<SandboxStream> {
        let s = self.connect()?;
        let mut w = s.try_clone().context("clone opencode stream socket")?;
        w.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n")
                .as_bytes(),
        )?;
        w.flush()?;
        // Consume the response headers; BufReader's leftover buffer holds any
        // body bytes already read, so it continues correctly as the body.
        let mut reader = BufReader::new(s.try_clone().context("clone opencode stream reader")?);
        let mut chunked = false;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            }
        }
        // opencode streams /event with `Transfer-Encoding: chunked`; de-chunk so
        // `drain_sse` sees clean SSE (curl does this for the docker path).
        let body: Box<dyn Read + Send> = if chunked {
            Box::new(ChunkedReader::new(reader))
        } else {
            Box::new(reader)
        };
        // Shut the socket down (via the retained `s` fd) on stop so the reader
        // thread's blocking read EOFs.
        let stopper = Box::new(move || {
            let _ = s.shutdown(Shutdown::Both);
        });
        Ok(SandboxStream { body, stopper })
    }
}

/// Decodes HTTP/1.1 `Transfer-Encoding: chunked` into the raw body stream:
/// `<hex-size>\r\n<size bytes>\r\n …  0\r\n\r\n`. Wraps the post-header
/// `BufReader` so a streaming consumer (`drain_sse`) reads only payload bytes.
struct ChunkedReader<R: BufRead> {
    inner: R,
    /// Unread bytes left in the current chunk.
    remaining: usize,
    done: bool,
}

impl<R: BufRead> ChunkedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            done: false,
        }
    }
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind};
        if self.done {
            return Ok(0);
        }
        if self.remaining == 0 {
            // Read the chunk-size line ("<hex>[;ext]\r\n").
            let mut line = String::new();
            if self.inner.read_line(&mut line)? == 0 {
                self.done = true;
                return Ok(0);
            }
            let hex = line.trim().split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(hex, 16)
                .map_err(|_| Error::new(ErrorKind::InvalidData, format!("bad chunk size: {line:?}")))?;
            if size == 0 {
                self.done = true; // last chunk; trailing CRLF/trailers ignored
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = self.remaining.min(buf.len());
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            self.done = true;
            return Ok(0);
        }
        self.remaining -= n;
        if self.remaining == 0 {
            // Consume the CRLF that terminates the chunk data.
            let mut crlf = [0u8; 2];
            let _ = self.inner.read_exact(&mut crlf);
        }
        Ok(n)
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
    fn missing_terminator_is_error() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nno end").is_err());
    }

    #[test]
    fn dechunks_sse_body() {
        use std::io::BufReader;
        // Two SSE events split across chunks, then the 0-terminator — exactly
        // opencode's /event framing (`59\r\ndata: {...}\n\n\r\n…`).
        let raw = "1c\r\ndata: {\"type\":\"connected\"}\n\n\r\n9\r\ndata: x\n\n\r\n0\r\n\r\n";
        let mut out = String::new();
        ChunkedReader::new(BufReader::new(raw.as_bytes()))
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "data: {\"type\":\"connected\"}\n\ndata: x\n\n");
    }
}
