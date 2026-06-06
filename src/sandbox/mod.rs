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

use anyhow::Result;

use crate::agents::{AgentSpec, RunOpts};
use crate::pillbox::Pillbox;

/// One backend = one way to provision a sandbox + vault session, inject
/// credentials, run the agent under a PTY, and wait for exit.
///
/// v0.6: takes a resolved [`Pillbox`] so the backend can locate the
/// auth home + vault state for the right scope.
pub(crate) trait SandboxBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()>;
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
