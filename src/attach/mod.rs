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
//!
//! Backends carry the identical [`frame::Frame`] codec over their own byte
//! pipe — docker exec stdio (local), ssh stdio (ssh), E2B's `pty.connect`
//! stream (e2b) — driving it with the shared pump in [`pump`]. The session
//! lifecycle is implemented per-backend as free functions
//! (`reattach`/`kill_session` in `sandbox/*`), not a trait.

pub(crate) mod frame;
pub(crate) mod host;
pub(crate) mod pump;
pub(crate) mod relay;
pub(crate) mod screen;
