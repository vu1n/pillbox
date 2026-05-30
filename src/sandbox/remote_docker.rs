//! `SandboxBackend` for a remote Docker daemon reached over SSH
//! transport — `pillbox run --remote docker://[user@]host[:port]`.
//!
//! This is the container-is-the-primitive backend the ssh/e2b paths
//! collapse onto (see [docs/remotes-redesign.md](../../docs/remotes-redesign.md)):
//! set `DOCKER_HOST=ssh://…`, run the existing runner image on that
//! daemon, attach over the existing docker-exec transport, and tar-cp the
//! cwd in/out instead of bind-mounting.
//!
//! **Status:** the URL is *accepted* end-to-end (parse → register → list →
//! info → backend selection), but the execution path is not built yet.
//! `run` returns a pointed error rather than silently misrouting to the
//! SSH backend, so `docker://` behaves honestly while the workspace-I/O
//! seam, sandbox-side vault wiring, and tar-cp contract land in follow-on
//! slices of the remotes collapse.

use anyhow::Result;

use super::SandboxBackend;
use crate::agents::{AgentSpec, RunOpts};
use crate::docker::DockerEndpoint;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::remote::{Remote, RemoteUrl};

pub(crate) struct RemoteDockerSandbox {
    remote: Remote,
}

impl RemoteDockerSandbox {
    pub(crate) fn new(remote: Remote) -> Self {
        Self { remote }
    }
}

impl SandboxBackend for RemoteDockerSandbox {
    fn run(&self, _spec: &AgentSpec, _opts: RunOpts, _resolved: &Pillbox) -> Result<()> {
        // Re-parse here (not at construction) so a hand-edited TOML
        // surfaces a pointed error rather than a generic connect failure —
        // mirrors the SSH/e2b backends' belt-and-suspenders re-validation.
        let endpoint = match self.remote.parsed_url().map_err(|e| {
            PillboxError::config(
                "run --remote (docker)",
                format!("remote `{}`: {e}", self.remote.name),
            )
        })? {
            RemoteUrl::Docker(d) => DockerEndpoint::remote(d.docker_host()),
            RemoteUrl::Ssh(_) | RemoteUrl::E2b(_) => {
                return Err(PillboxError::config(
                    "run --remote (docker)",
                    format!("remote `{}` is not a docker:// URL", self.remote.name),
                )
                .into());
            }
        };

        // The placement axis is wired (DOCKER_HOST → the remote daemon);
        // the container lifecycle + workspace materialization (overlay-CoW
        // over rustic-local-on-remote) are the next slice. Report honestly
        // with the resolved endpoint rather than misrouting to SSH.
        let docker_host = endpoint.host_override().unwrap_or("(local)");
        Err(PillboxError::runtime(
            "run --remote (docker)",
            format!(
                "the docker:// backend is not built yet (would target DOCKER_HOST={docker_host})"
            ),
        )
        .with_next(
            "pillbox run --remote ssh://…  # use the ssh backend until docker:// lands".to_string(),
        )
        .into())
    }
}
