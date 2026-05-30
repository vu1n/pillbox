//! `SandboxBackend` for a remote Docker daemon reached over SSH transport —
//! `pillbox run --remote docker://[user@]host[:port]`.
//!
//! The container-is-the-primitive backend the ssh/e2b paths collapse onto
//! (see [docs/remotes-redesign.md](../../docs/remotes-redesign.md)): point
//! `DOCKER_HOST=ssh://…` at the daemon, run the existing runner image there,
//! and attach over the existing docker-exec transport. Unlike the ssh path
//! (nested docker, S3 hydrate) this uses the **direct** vault mechanism —
//! the agent runs straight in the runner container with a sidecar proxy —
//! and the **pre-staged** workspace: `launch_staged_container` tar-cp's the
//! cwd in (no S3, no bind-mount). The run assembly is the create → stage →
//! start → attach lifecycle from the run-assembly state machine in
//! remotes-redesign.md.
//!
//! **Status (milestone 1):** foreground runs execute the agent in the remote
//! container with auth + vault forwarded via the blob, attach over the
//! endpoint, and reap on exit. Deferred (clearly-marked follow-ons): host-side
//! **result extraction** (`docker cp` the workspace out → cwd + snapshot — the
//! `ResultCaptured` state), **creds read-back** (the `CredsPersisted`-before-
//! `TornDown` invariant / 2nd-run-401 guard), `--detach` (+ `session
//! attach/rm` re-resolution), and OTEL env forwarding for sandbox-side obs.

use anyhow::{Context, Result};

use super::container::launch_staged_container;
use super::vault_stdin::{build_vault_stdin_blob, WorkspaceProvision};
use super::SandboxBackend;
use crate::agents::{base_docker_args_create, AgentSpec, RunOpts, GUEST_WORKSPACE};
use crate::attach::pump::{self, Outcome};
use crate::docker::{self, DockerEndpoint};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::remote::{Remote, RemoteUrl};
use crate::session::Session;

const ACTION: &str = "run --remote (docker)";

/// Where the in-container pty-host listens; the per-attach relay (run via
/// `docker exec` over the endpoint) connects to the same path.
const ATTACH_SOCK: &str = "/tmp/pillbox-attach.sock";

/// Force-removes its container on drop, on the *endpoint's* daemon — so a
/// foreground run's container is reaped on every exit path (normal, early
/// `?`-return, panic). Mirrors `local_docker::ContainerGuard` but
/// endpoint-aware.
struct ContainerGuard(DockerEndpoint, String);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = docker::rm_force_at(&self.0, &self.1);
    }
}

pub(crate) struct RemoteDockerSandbox {
    remote: Remote,
}

impl RemoteDockerSandbox {
    pub(crate) fn new(remote: Remote) -> Self {
        Self { remote }
    }
}

impl SandboxBackend for RemoteDockerSandbox {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Re-parse here (not at construction) so a hand-edited TOML surfaces a
        // pointed error rather than a generic connect failure — mirrors the
        // ssh/e2b backends' belt-and-suspenders re-validation.
        let endpoint = match self.remote.parsed_url().map_err(|e| {
            PillboxError::config(ACTION, format!("remote `{}`: {e}", self.remote.name))
        })? {
            RemoteUrl::Docker(d) => DockerEndpoint::remote(d.docker_host()),
            RemoteUrl::Ssh(_) | RemoteUrl::E2b(_) => {
                return Err(PillboxError::config(
                    ACTION,
                    format!("remote `{}` is not a docker:// URL", self.remote.name),
                )
                .into());
            }
        };

        // Milestone 1 is foreground-only. Detach (and `session attach/rm`
        // re-resolution for inline-URL docker:// sessions) is a follow-on.
        if opts.detach {
            return Err(
                PillboxError::usage(ACTION, "--detach is not supported for docker:// yet")
                    .with_next("run docker:// in the foreground for now")
                    .into(),
            );
        }

        // Preflight the REMOTE daemon + image (over the endpoint), so a cold
        // host / missing image fails with a pointed error before we build the
        // secret-bearing blob.
        let runner_image = docker::check_ready_for_at(resolved, &endpoint).map_err(|e| {
            // The image must exist on the *remote* daemon; make the generic
            // `docker pull <image>` hint endpoint-aware. `host_override()`
            // already carries the full `ssh://…` DOCKER_HOST value — don't
            // re-prefix it.
            let next = match endpoint.host_override() {
                Some(docker_host) => format!(
                    "DOCKER_HOST={docker_host} docker pull <runner-image>  # put the image on the remote daemon"
                ),
                None => "docker pull <runner-image>".to_string(),
            };
            PillboxError::resource(ACTION, format!("{e}")).with_next(next)
        })?;

        // Auth is forwarded into the container via the blob (the direct path
        // has no pre-existing login). Check host-side credentials up front so
        // the failure is "log in", not a downstream "blob carries no auth".
        let home = spec.home_dir(resolved)?;
        if !home.join(spec.cred_sentinel).exists() {
            return Err(PillboxError::runtime(
                ACTION,
                format!("no stored credentials for `{}`", spec.id),
            )
            .with_next(format!("pillbox auth login --agent {}", spec.id))
            .into());
        }

        let workspace_host = match &opts.workspace {
            Some(p) => p.clone(),
            None => std::env::current_dir().context("resolve current working directory")?,
        };

        let session_id = Session::new_id();

        // Build the pre-staged (no-S3) blob: the workspace is tar-cp'd into the
        // container at GUEST_WORKSPACE, so the in-container direct dispatch runs
        // the agent there and skips S3 hydrate/push (results come back
        // host-side). The blob carries the agent auth + vault config so the
        // sandbox-side proxy + agent "just work" without a host-side proxy.
        let mut blob = build_vault_stdin_blob(
            spec,
            &opts,
            resolved,
            "run --remote (docker)",
            WorkspaceProvision::PreStaged {
                container_dir: GUEST_WORKSPACE.to_string(),
            },
        )?;
        blob.context = crate::vault::RunContext {
            session_id: Some(session_id.clone()),
            mode: Some(crate::vault::RunContext::mode_for(opts.detach).to_string()),
            workspace_id: Some(resolved.workspace_id().to_string()),
        };

        // Stage the blob to a host 0600 tempfile; `launch_staged_container`
        // `docker cp`s it into the container before start.
        let blob_bytes = blob.to_bytes()?;
        let mut tmp = tempfile::Builder::new()
            .prefix("pillbox-docker-blob-")
            .suffix(".json")
            .tempfile()
            .map_err(|e| PillboxError::runtime(ACTION, format!("stage blob: {e}")))?;
        {
            use std::io::Write as _;
            tmp.as_file_mut()
                .write_all(&blob_bytes)
                .and_then(|()| tmp.as_file_mut().sync_all())
                .map_err(|e| PillboxError::runtime(ACTION, format!("write staged blob: {e}")))?;
        }
        let blob_container_path = format!("/tmp/pillbox-blob-{session_id}.json");

        // Container command: the pty-host owns the PTY and wraps the direct
        // run, which reads the blob, materializes auth, starts the sidecar
        // vault proxy, and execs the agent against the pre-staged workspace.
        let mut args = base_docker_args_create();
        args.push(runner_image);
        args.extend([
            "pillbox".into(),
            "pty-host".into(),
            "--sock".into(),
            ATTACH_SOCK.into(),
            "--".into(),
            "pillbox".into(),
            "run".into(),
            "--vault-stdin-direct".into(),
            "--blob-file".into(),
            blob_container_path.clone(),
        ]);

        eprintln!(
            "pillbox: connecting to `{}` ({}) …",
            self.remote.name, self.remote.url
        );

        // create → stage workspace (tar-cp) + blob (docker cp) → start.
        let launched = launch_staged_container(
            &endpoint,
            &args,
            &workspace_host,
            GUEST_WORKSPACE,
            Some((tmp.path(), &blob_container_path)),
        )?;
        let container = launched.container;
        // No silent caps: report any secrets the ingest dropped.
        if !launched.stage.excluded_secrets.is_empty() {
            eprintln!(
                "pillbox: note: {} secret path(s) excluded from the workspace transfer",
                launched.stage.excluded_secrets.len()
            );
        }

        // Foreground: reap the container on every exit path. (A future
        // `--detach` keeps it alive + records a Session instead.)
        let _guard = ContainerGuard(endpoint.clone(), container.clone());

        let outcome = attach_via_exec_at(&endpoint, &container, false);

        // TODO(result extraction — `ResultCaptured`): `docker cp`
        //   <container>:GUEST_WORKSPACE out → land in cwd + snapshot host-side.
        // TODO(creds read-back — `CredsPersisted` before `TornDown`): copy the
        //   container's refreshed auth out → persist to the global store, or a
        //   second run gets stale tokens → 401. Must run BEFORE the guard reaps.

        // A fast, non-interactive agent (e.g. `claude -p`) can exit before the
        // attach relay connects; the relay then can't reach a stopped container
        // and the pump sees an immediate `Disconnected`. The container's own
        // exit code is the authoritative result in that case — prefer it over
        // the pump outcome so the run reports the agent's status, not a relay
        // hiccup. (Streaming a fast agent's *output* is the deferred
        // result-capture path; headless docker:// will surface it via pull.)
        if let Some(code) = docker::container_exit_code_at(&endpoint, &container) {
            return match code {
                0 => Ok(()),
                c => Err(PillboxError::runtime(
                    ACTION,
                    format!("{} exited with status {c}", spec.id),
                )
                .into()),
            };
        }

        match outcome? {
            Outcome::Exited(0) | Outcome::Detached | Outcome::Disconnected => Ok(()),
            Outcome::Exited(code) => Err(PillboxError::runtime(
                ACTION,
                format!("{} exited with status {code}", spec.id),
            )
            .into()),
        }
    }
}

/// Attach the terminal pump to the running pty-host by execing the per-attach
/// relay over the endpoint and pumping its stdio. Mirrors
/// `local_docker::attach_via_exec` but endpoint-aware (the exec stream rides
/// `DOCKER_HOST=ssh://…` back to the host pump unchanged).
fn attach_via_exec_at(
    endpoint: &DockerEndpoint,
    container: &str,
    detach_enabled: bool,
) -> Result<Outcome> {
    let mut child = docker::exec_attach_at(
        endpoint,
        container,
        &[
            "pillbox".into(),
            "pty-relay".into(),
            "--sock".into(),
            ATTACH_SOCK.into(),
        ],
    )?;
    let stdout = child.stdout.take().context("docker exec relay stdout")?;
    let stdin = child.stdin.take().context("docker exec relay stdin")?;
    let outcome = pump::attach_terminal(stdout, stdin, detach_enabled)?;
    let _ = child.kill();
    let _ = child.wait();
    Ok(outcome)
}
