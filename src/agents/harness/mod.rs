//! Harness adapters — per-coding-harness integration behind the contract.
//!
//! A *harness* is a coding-agent CLI; each speaks its own structured-output
//! dialect, and an adapter normalizes that to the canonical [`crate::contract`]
//! events. Two trait flavours, split by transport:
//!   - [`HarnessAdapter`] — the run streams JSON lines over stdout (claude
//!     `-p`, pi `--mode json`); driven by `AgentDriver` in `commands/sandbox`.
//!   - [`ServeAdapter`] — the harness runs an HTTP server with an SSE stream
//!     (opencode `serve`); driven by `ServeDriver`.
//!
//! Each harness lives in its own submodule, co-locating its adapter, its
//! normalizer state, its helpers, and its tests.

use serde_json::Value;

use crate::contract::Payload;

mod claude;
mod opencode;
mod pi;

pub(crate) use claude::ClaudeAdapter;
pub(crate) use opencode::OpencodeAdapter;
pub(crate) use pi::PiAdapter;

/// A harness whose headless run streams structured JSON **lines over stdout**
/// (claude `-p`, pi `--mode json`). The adapter carries its own state across
/// lines, so a fresh adapter is one run.
pub(crate) trait HarnessAdapter {
    /// argv for a headless, structured-output run of `prompt`, exec'd inside
    /// the sandbox. Must run non-interactively and auto-allow tools (the
    /// sandbox is the security boundary).
    fn run_argv(&self, prompt: &str) -> Vec<String>;

    /// Map one line of the harness's structured stdout to zero or more
    /// contract events.
    fn parse_line(&mut self, line: &Value) -> Vec<Payload>;
}

/// A harness that runs an **HTTP server** (`opencode serve`: REST + an SSE
/// event stream) rather than streaming JSON over stdout. The transport differs,
/// but the normalizer concept is identical to [`HarnessAdapter::parse_line`]:
/// one harness event → zero-or-more contract [`Payload`]s.
pub(crate) trait ServeAdapter {
    /// argv that starts the harness's HTTP server inside the sandbox, bound to
    /// `port` on loopback; the `ServeDriver` then talks to it over the
    /// container's own loopback.
    fn serve_argv(&self, port: u16) -> Vec<String>;

    /// Map one harness SSE event (`{id, type, properties}`) to zero or more
    /// contract events.
    fn parse_event(&mut self, event: &Value) -> Vec<Payload>;

    /// Did this event signal the agent turn is complete? The driver stops
    /// consuming the stream once a terminal event arrives.
    fn is_terminal(&self, event: &Value) -> bool;
}

/// Resolve a stdout-streaming harness adapter by agent id.
pub(crate) fn lookup(id: &str) -> Option<Box<dyn HarnessAdapter>> {
    match id {
        "claude" => Some(Box::new(ClaudeAdapter::default())),
        "pi" => Some(Box::new(PiAdapter::default())),
        _ => None,
    }
}

/// Resolve a serve-based harness adapter by agent id.
pub(crate) fn lookup_serve(id: &str) -> Option<Box<dyn ServeAdapter>> {
    match id {
        "opencode" => Some(Box::new(OpencodeAdapter::default())),
        _ => None,
    }
}

/// Borrow a string field as `&str`, or `""` — the one stateless helper shared
/// by every adapter's normalizer.
fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
