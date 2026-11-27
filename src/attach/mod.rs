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
//! The libkrun backend carries the [`frame::Frame`] codec over its persistent
//! attach socket, driving it with the shared pump in [`pump`]. The session
//! lifecycle is the backend's [`crate::sandbox::LiveSession`].

pub(crate) mod driver;
pub(crate) mod frame;
pub(crate) mod host;
pub(crate) mod pump;
pub(crate) mod screen;
