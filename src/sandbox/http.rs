//! `SandboxHttp` — make HTTP requests to a localhost service running *inside* a
//! sandbox.
//!
//! The opencode [`Integration::Server`](crate::agents::Integration) bridge
//! (`sandbox::opencode`) drives a headless `opencode serve` over its HTTP API:
//! poll readiness (`GET /api/info`), create a session, push prompts (the turn's
//! events are read from the guest capture file, not over this seam). Reaching
//! an in-sandbox HTTP server is the primitive — and the one the documented gateway / multiplayer / §0 use cases all want (proxy the API,
//! fan the event stream out to remote participants). This trait is that seam,
//! so the bridge speaks HTTP once and each backend supplies the transport:
//!
//! - **libkrun** — a real HTTP/1.1 client over a vsock socket the guest
//!   forwards to `127.0.0.1:<port>`. Server agents are libkrun-only; docker
//!   has no implementation.
//!
//! Chosen over a generic "run a command in the sandbox" exec channel: the use
//! cases need HTTP to one in-guest server, not arbitrary command exec, and the
//! port-forward this implies is the same primitive web-attach/multiplayer want.

use anyhow::Result;

/// One HTTP response from an in-sandbox server.
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: String,
}

/// HTTP client to a single localhost service inside one running sandbox.
pub(crate) trait SandboxHttp {
    /// One-shot request; returns the status code and body. `json_body`, when
    /// present, is sent as `content-type: application/json`.
    fn request(&self, method: &str, path: &str, json_body: Option<&str>) -> Result<HttpResponse>;
}
