//! Sandbox backends — the surface that actually runs a configured
//! [`AgentSpec`] for `pillbox run`.
//!
//! v0.6 splits the run path off of `AgentSpec` into a trait so we can
//! pick at run time between executing the agent locally (Docker) and
//! remotely (SSH to a VPS, later E2B managed cloud).
//!
//! - PR 1 — trait + `LocalDocker` impl, no remote backends.
//! - PR 4 — `RemoteSshSandbox` impl: ssh into a registered VPS, run a
//!   pillbox sandbox **there**, proxy stdio back to the local terminal.
//!   See [`remote_ssh`].

pub(crate) mod local_docker;
pub(crate) mod remote_ssh;

use anyhow::Result;

use crate::agents::{AgentSpec, RunOpts};
use crate::pillbox::Pillbox;
use crate::remote::Remote;

/// One backend = one way to provision a sandbox + vault session, inject
/// credentials, run the agent under a PTY, and wait for exit.
///
/// v0.6: takes a resolved [`Pillbox`] so the backend can locate the
/// auth home + vault state for the right scope.
pub(crate) trait SandboxBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()>;
}

/// Pick a backend for one `pillbox run` invocation. Centralized here so
/// the trait shape and the selection rule live next to each other —
/// when v0.6 PR 5 adds E2B, this is the only place that grows a new arm.
pub(crate) fn select_backend(remote: Option<Remote>) -> Box<dyn SandboxBackend> {
    match remote {
        Some(r) => Box::new(remote_ssh::RemoteSshSandbox::new(r)),
        None => Box::new(local_docker::LocalDocker),
    }
}
