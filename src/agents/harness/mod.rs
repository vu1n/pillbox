//! Harness adapters — per-coding-harness integration behind the contract.
//!
//! A *harness* is a coding-agent CLI; each speaks its own structured-output
//! dialect, and an adapter normalizes that to the canonical [`crate::contract`]
//! events. [`HarnessAdapter`] covers harnesses whose run streams JSON lines
//! over stdout (claude `-p`, cursor, pi `--mode json`); `AgentDriver` in
//! `commands/sandbox` drives it. Server harnesses (opencode, codex-serve) have
//! no adapter here: they run on the libkrun backend and are read through the
//! §0 event bridge (`sandbox::opencode`, `events::codex_serve`).
//!
//! Each harness lives in its own submodule, co-locating its adapter, its
//! normalizer state, its helpers, and its tests.

use serde_json::Value;

use crate::contract::Payload;

mod claude;
mod cursor;
mod pi;

pub(crate) use claude::ClaudeAdapter;
pub(crate) use cursor::CursorAdapter;
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

/// Resolve a stdout-streaming harness adapter by agent id.
pub(crate) fn lookup(id: &str) -> Option<Box<dyn HarnessAdapter>> {
    match id {
        "claude" => Some(Box::new(ClaudeAdapter::default())),
        "cursor" => Some(Box::new(CursorAdapter::default())),
        "pi" => Some(Box::new(PiAdapter::default())),
        _ => None,
    }
}

/// Borrow a string field as `&str`, or `""` — the one stateless helper shared
/// by every adapter's normalizer.
fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
