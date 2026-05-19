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

/// One backend = one way to provision a sandbox + vault session, inject
/// credentials, run the agent under a PTY, and wait for exit.
///
/// Methods take `&self` rather than `self` so the same backend can be
/// reused across multiple runs without rebuilding (e.g. an SSH backend
/// will eventually hold a connection pool).
pub(crate) trait SandboxBackend {
    /// Equivalent to today's `AgentSpec::run`: end-to-end run.
    fn run(&self, spec: &AgentSpec, opts: RunOpts) -> Result<()>;
}
