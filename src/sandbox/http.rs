//! `SandboxHttp` — make HTTP requests to a localhost service running *inside* a
//! sandbox.
//!
//! The opencode [`Integration::Server`](crate::agents::Integration) bridge
//! (`sandbox::opencode`) drives a headless `opencode serve` over its HTTP API:
//! poll readiness (`GET /doc`), create a session, push prompts, stream
//! `/event`. Reaching an in-sandbox HTTP server is the primitive — and the one
//! the documented gateway / multiplayer / §0 use cases all want (proxy the API,
//! fan the event stream out to remote participants). This trait is that seam,
//! so the bridge speaks HTTP once and each backend supplies the transport:
//!
//! - **docker** — `docker exec curl 127.0.0.1:<port>` (one `exec` per call).
//! - **libkrun** (step 7c) — a real HTTP/1.1 client over a vsock socket the
//!   guest forwards to `127.0.0.1:<port>`.
//!
//! Chosen over a generic "run a command in the sandbox" exec channel: the use
//! cases need HTTP to one in-guest server, not arbitrary command exec, and the
//! port-forward this implies is the same primitive web-attach/multiplayer want.

use std::io::Read;

use anyhow::{Context, Result};

use crate::docker::{self, DockerEndpoint};

/// One HTTP response from an in-sandbox server.
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: String,
}

/// A streaming response body (e.g. an SSE `/event` stream): the HTTP headers
/// are already stripped by the impl, so `body` yields the raw event bytes. The
/// `stopper` tears the transport down (kills the `docker exec` / closes the
/// vsock) so a reader thread parked on it returns EOF. Separable from any spawn
/// handle on purpose — a streaming consumer hands `body` to a thread and keeps
/// `stopper` in a [`TailerHandle`](crate::events::transcripts::TailerHandle).
pub(crate) struct SandboxStream {
    pub(crate) body: Box<dyn Read + Send>,
    pub(crate) stopper: Box<dyn FnOnce() + Send>,
}

/// HTTP client to a single localhost service inside one running sandbox.
pub(crate) trait SandboxHttp {
    /// One-shot request; returns the status code and body. `json_body`, when
    /// present, is sent as `content-type: application/json`.
    fn request(&self, method: &str, path: &str, json_body: Option<&str>) -> Result<HttpResponse>;

    /// Open a streaming `GET` (the `/event` SSE stream). The returned body
    /// yields response bytes past the headers.
    fn open_stream(&self, path: &str) -> Result<SandboxStream>;
}

/// `SandboxHttp` over a docker daemon: every call is a `docker exec curl` to
/// `127.0.0.1:<port>` on `endpoint` (local, or a `docker://` remote where
/// `DOCKER_HOST=ssh://…` carries the exec stream back over SSH). No port
/// publishing — curl runs inside the same container as the server.
pub(crate) struct DockerHttp {
    endpoint: DockerEndpoint,
    container: String,
    port: u16,
}

impl DockerHttp {
    pub(crate) fn new(endpoint: DockerEndpoint, container: String, port: u16) -> Self {
        Self {
            endpoint,
            container,
            port,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Spawn `curl <args>` in the container with stdin closed, stdout piped.
    fn spawn_curl(&self, args: &[&str]) -> Result<std::process::Child> {
        let mut argv = vec!["curl".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let mut child = docker::exec_attach_at(&self.endpoint, &self.container, &argv)?;
        drop(child.stdin.take());
        Ok(child)
    }
}

impl SandboxHttp for DockerHttp {
    fn request(&self, method: &str, path: &str, json_body: Option<&str>) -> Result<HttpResponse> {
        let url = self.url(path);
        // `-s` silent (wait_ready polls before the server binds — expected
        // connect failures shouldn't spew); `-w '\n%{http_code}'` appends the
        // status as a final line we split off (the body never owns the last
        // line, since we always add one).
        let mut args = vec!["-s", "-w", "\n%{http_code}", "-X", method];
        if let Some(body) = json_body {
            args.extend(["-H", "content-type: application/json", "-d", body]);
        }
        args.push(&url);
        let mut child = self.spawn_curl(&args)?;
        let mut out = String::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout.read_to_string(&mut out).ok();
        }
        let _ = child.wait();
        let (body, code) = out.rsplit_once('\n').unwrap_or(("", out.trim()));
        Ok(HttpResponse {
            status: code.trim().parse().unwrap_or(0),
            body: body.to_string(),
        })
    }

    fn open_stream(&self, path: &str) -> Result<SandboxStream> {
        let url = self.url(path);
        // `curl -N` disables buffering and strips the HTTP headers, so stdout is
        // the raw SSE body — exactly what `drain_sse` expects.
        let mut child = self.spawn_curl(&["-sN", &url])?;
        let stdout = child.stdout.take().context("docker exec curl stdout")?;
        Ok(SandboxStream {
            body: Box::new(stdout),
            // The blocking read can't observe a flag mid-frame, so stop = kill
            // the exec to EOF the pipe, then reap it.
            stopper: Box::new(move || {
                let _ = child.kill();
                let _ = child.wait();
            }),
        })
    }
}
