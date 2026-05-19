//! Sandbox backends — the surface that actually runs a configured
//! [`AgentSpec`] for `pillbox <agent> run`.
//!
//! v0.6 splits the run path off of `AgentSpec` into a trait so we can
//! pick at run time between executing the agent locally (today, Docker)
//! and remotely (PR 2: SSH to a VPS or E2B managed cloud). PR 1 ships
//! only the trait + the `LocalDocker` impl that wraps the historical
//! Docker logic verbatim, and a future PR validates the trait shape
//! against `RemoteSsh`.

pub(crate) mod local_docker;

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
