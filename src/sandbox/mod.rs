//! Sandbox backends — the surface that actually runs a configured
//! [`AgentSpec`] for `pillbox run`.
//!
//! The run path is split off of `AgentSpec` into a trait so we can pick
//! at run time between the local backends. The deprecated remote backends
//! (ssh/docker/e2b) were removed in the libkrun pivot — "remote" is
//! becoming the managed/Cloudflare tier; until then pillbox is local-only.
//!
//! - [`docker::DockerBackend`] — host Docker daemon (the default).
//! - [`libkrun::LibkrunBackend`] — a local libkrun microVM (feature-gated
//!   `libkrun`; opt in via `PILLBOX_BACKEND=libkrun`).

pub(crate) mod appserver;
pub(crate) mod appserver_client;
pub(crate) mod docker;
pub(crate) mod http;
#[cfg(feature = "libkrun")]
pub(crate) mod libkrun;
pub(crate) mod opencode;

use std::path::PathBuf;

use anyhow::Result;

use crate::agents::{AgentSpec, RunOpts};
use crate::events::source::EventSource;
use crate::events::transcripts::TailerHandle;
use crate::pillbox::Pillbox;
use crate::sandbox::http::SandboxHttp;

/// What a backend can do — **declared, not chased**. The plane queries this to
/// decide whether a verb is available on the current substrate, instead of
/// discovering a `bail!("docker only")` buried in a command. Default = all
/// false: a backend opts into each capability it actually supports. The matrix
/// is in docs/substrate-plane.md.
//
// Declared ahead of its consumers (the dispatch sites that will read it), so
// allow(dead_code) until those are ported onto the plane.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    /// `session send` into a PTY agent.
    pub(crate) pty_drive: bool,
    /// Live `watch`/`subscribe`/`wait-idle` for a PTY agent.
    pub(crate) live_pty_tail: bool,
    /// `Integration::Server` agents (opencode, codex-serve).
    pub(crate) server_mode: bool,
    /// `sandbox spawn`/`exec`/`agent` — a long-lived exec target.
    pub(crate) long_lived_exec: bool,
    /// `score --in-sandbox` + `--grader-egress` (a one-shot grader VM).
    pub(crate) in_sandbox_grading: bool,
    /// DNS-level egress allow/deny (not just proxy-level).
    pub(crate) real_egress_fence: bool,
    /// `--detach` + `--vault` together (the vault outlives the CLI).
    pub(crate) detached_vault: bool,
    /// Post-hoc `ingest` of a headless capture (no live host tailer).
    pub(crate) post_hoc_ingest: bool,
}

/// One backend = one way to provision a sandbox + vault session, inject
/// credentials, run the agent, and supervise it.
///
/// `run()` is the existing foreground/detach entry point. `start()` (wired in
/// Phase 1+) returns a [`LiveSession`] — the polymorphic control surface the
/// command layer drives without branching on the backend. See
/// docs/substrate-plane.md. Takes a resolved [`Pillbox`] so the backend can
/// locate the auth home + vault state for the right scope.
pub(crate) trait SandboxBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()>;

    /// What this backend can do — queried before (or instead of) attempting a
    /// verb. The honest current-state profile, per docs/substrate-plane.md.
    // No caller until the dispatch sites are ported onto the plane.
    #[allow(dead_code)]
    fn capabilities(&self) -> Caps;

    /// Provision + return the live-session handle the command layer drives.
    /// The default bails so adding this method leaves existing `run()` callers
    /// untouched until a backend overrides it.
    #[allow(dead_code)]
    fn start(
        &self,
        _spec: &AgentSpec,
        _opts: RunOpts,
        _resolved: &Pillbox,
    ) -> Result<Box<dyn LiveSession>> {
        anyhow::bail!("this backend has no LiveSession::start yet (see docs/substrate-plane.md)")
    }
}

/// The live session — **the plane**. A running (foreground or detached) agent
/// the command layer drives and reads *without branching on the backend*: every
/// `session send`/`attach`/`watch`/`kill`/… resolves to one of these. Each
/// method's availability is gated by [`Caps`] — an unsupported verb returns a
/// clear error rather than being a missing match arm. Object-safe, so it rides
/// `Box<dyn LiveSession>`.
///
/// The method set maps 1:1 to the backend-string dispatch sites it replaces
/// (`send`/`attach`/`kill`/`event_source`/`http`/`ingest`).
//
// Unused until those dispatch sites are ported onto it, so allow(dead_code).
#[allow(dead_code)]
pub(crate) trait LiveSession {
    /// This session's backend capabilities (the per-session view of [`Caps`]).
    fn caps(&self) -> Caps;

    /// Drive: push bytes to the agent (PTY input / server prompt). `caps().pty_drive`.
    fn send(&self, bytes: &[u8]) -> Result<()>;

    /// Reattach a terminal to this session.
    fn attach(&self, resolved: &Pillbox) -> Result<()>;

    /// Open the §0 read source plus the tailer that fills it for a live session
    /// (held by the caller for the stream's lifetime; `None` when a producer
    /// already tails). Backend-blind replacement for `resolve_streaming_session`.
    fn event_source(
        &self,
        resolved: &Pillbox,
    ) -> Result<(Box<dyn EventSource + Send>, Option<TailerHandle>)>;

    /// HTTP handle to a server-mode agent's in-sandbox server. `caps().server_mode`.
    fn http(&self) -> Result<Box<dyn SandboxHttp>>;

    /// Host path of this session's (result) workspace.
    fn workspace_path(&self) -> Result<PathBuf>;

    /// Drain a headless capture into the durable log post-hoc. `caps().post_hoc_ingest`.
    fn ingest(&self, resolved: &Pillbox) -> Result<usize>;

    /// Tear down the backend (kill sandbox/VM) and release its artifacts.
    fn kill(&self, resolved: &Pillbox) -> Result<()>;
}

/// Pick the local backend for one `pillbox run`. The deprecated remote backends
/// (ssh/docker/e2b) were removed in the libkrun pivot — "remote" is becoming the
/// managed/Cloudflare tier; until then pillbox is local-only. libkrun (microVM)
/// opts in via `PILLBOX_BACKEND=libkrun` (feature-gated); the default is Docker.
pub(crate) fn select_backend() -> Box<dyn SandboxBackend> {
    #[cfg(feature = "libkrun")]
    if std::env::var_os("PILLBOX_BACKEND").is_some_and(|v| v == "libkrun") {
        return Box::new(libkrun::LibkrunBackend);
    }
    Box::new(docker::DockerBackend)
}
