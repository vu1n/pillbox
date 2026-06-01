//! Sandbox backends — the surface that actually runs a configured
//! [`AgentSpec`] for `pillbox run`.
//!
//! The run path is split off of `AgentSpec` into a trait so we can
//! pick at run time between executing the agent locally (Docker) and
//! remotely (SSH to a VPS, E2B managed cloud).
//!
//! - [`local_docker::LocalDocker`] — host Docker daemon.
//! - [`remote_docker::RemoteDockerSandbox`] — a remote Docker daemon over
//!   SSH transport (`DOCKER_HOST=ssh://…`). Selected for the `docker://`
//!   scheme; the container-is-the-primitive backend the ssh/e2b paths
//!   collapse onto (see docs/remotes-redesign.md).
//! - [`remote_ssh::RemoteSshSandbox`] — ssh into a registered VPS,
//!   run a pillbox sandbox there, proxy stdio back.
//! - [`remote_e2b::RemoteE2bSandbox`] — spawn an E2B managed sandbox
//!   via the `@e2b/code-interpreter` JS SDK (bundled Node helper).
//!   Selected when the remote URL has the `e2b://` scheme; an unknown /
//!   hand-edited scheme falls through to SSH.

pub(crate) mod container;
pub(crate) mod local_docker;
pub(crate) mod opencode;
pub(crate) mod remote_docker;
pub(crate) mod remote_e2b;
pub(crate) mod remote_ssh;
pub(crate) mod vault_stdin;
pub(crate) mod workspace_stage;

use anyhow::Result;

use crate::agents::{AgentSpec, RunOpts};
use crate::pillbox::Pillbox;
use crate::remote::{Remote, RemoteUrl};

/// One backend = one way to provision a sandbox + vault session, inject
/// credentials, run the agent under a PTY, and wait for exit.
///
/// v0.6: takes a resolved [`Pillbox`] so the backend can locate the
/// auth home + vault state for the right scope.
pub(crate) trait SandboxBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()>;
}

/// Pick a backend for one `pillbox run` invocation. The parsed
/// [`RemoteUrl`] variant is the discriminator — keeps the `"ssh://"` /
/// `"e2b://"` literals out of the dispatcher. A bad URL here would have
/// been caught at `remote add` time and again at registry read, but
/// the backend constructors re-validate inside `run` so a hand-edited
/// TOML still fails with a pointed error rather than a generic "could
/// not connect".
pub(crate) fn select_backend(remote: Option<Remote>) -> Box<dyn SandboxBackend> {
    let Some(r) = remote else {
        return Box::new(local_docker::LocalDocker);
    };
    // Unknown / malformed URL → fall through to SSH so the backend's own
    // re-parse produces the actionable error. The registry's `parse_remote`
    // already rejects unknown schemes at load time, so this branch is only
    // ever taken on hand-edited TOML.
    match r.parsed_url() {
        Ok(RemoteUrl::E2b(_)) => Box::new(remote_e2b::RemoteE2bSandbox::new(r)),
        Ok(RemoteUrl::Docker(_)) => Box::new(remote_docker::RemoteDockerSandbox::new(r)),
        Ok(RemoteUrl::Ssh(_)) | Err(_) => Box::new(remote_ssh::RemoteSshSandbox::new(r)),
    }
}
