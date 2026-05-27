//! Interactive attach transport — the PTY data plane.
//!
//! This is the sibling of the PTY-free structured channel in
//! `proto/pillbox/v1/agent.proto`: where that channel is for consumers
//! that render *semantics* (orchestrators, chat, hermes), this is for
//! consumers that render a real *terminal* (orca, interactive lum, a
//! human). It formalizes what `docs/agent-io-contract.md` defers to as
//! "the existing attach transport". Full design: `docs/attach-transport.md`.
//!
//! Layering:
//!   - [`frame`] — the backend-agnostic wire codec (data plane).
//!   - [`screen`] — [`screen::ScreenModel`], the sandbox-side vt100 screen
//!     used to serve a bounded ANSI snapshot on attach.
//!   - the traits below — the cross-backend session contract. Backends
//!     supply only a byte pipe ([`FramePipe`]); the frames are identical
//!     everywhere.
//!
//! Phase 1 (this commit) lands the contract + screen model + codec with
//! tests and no behavior change. Backends implement [`SessionBackend`] and
//! the shared pump arrives in later phases (see `docs/attach-transport.md`).

pub(crate) mod frame;
pub(crate) mod screen;

use std::io::{Read, Write};

use anyhow::Result;

use crate::agents::{AgentSpec, RunOpts};
use crate::pillbox::Pillbox;
use crate::session::Session;

/// Any bidirectional byte pipe a backend can open to a session's
/// in-sandbox pty-host. The [`frame::Frame`] codec runs over it
/// unchanged regardless of how the bytes are carried:
///   - local docker: `docker exec` stdio
///   - e2b:          `pty.connect` stream
///   - ssh:          ssh stdio
///
/// Blanket-implemented for anything that is `Read + Write + Send`.
#[allow(dead_code)] // implementors land with the backends (phases 2–4)
pub(crate) trait FramePipe: Read + Write + Send {}
impl<T: Read + Write + Send> FramePipe for T {}

/// A backend that can run an agent in a persistent, named PTY session and
/// re-open a frame pipe to it later. This lifts today's e2b-only
/// `reattach`/`kill_session` free functions (`sandbox/remote_e2b.rs`) into
/// one contract and adds `launch`, so detach/reattach is uniform across
/// e2b and ssh. `local_docker` may skip it for the non-detached path,
/// where the host terminal already *is* the PTY.
#[allow(dead_code)] // implemented per-backend in later phases
pub(crate) trait SessionBackend {
    /// Provision the sandbox and launch the in-sandbox pty-host running
    /// the agent. Returns once the host is up and reachable; the sandbox
    /// keeps running so the session can be attached, detached, reattached.
    fn launch(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<Session>;

    /// Open a frame pipe to an existing session's pty-host. The caller
    /// drives it with the shared pump (terminal or embedder front-end).
    fn attach(&self, session: &Session) -> Result<Box<dyn FramePipe>>;

    /// Tear down the sandbox backing this session.
    fn kill(&self, session: &Session) -> Result<()>;
}
