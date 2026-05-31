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
//! **Status:** foreground runs execute the agent in the remote container with
//! auth + vault forwarded via the blob, attach over the endpoint, and reap on
//! exit. `--detach` records a [`Session`] and leaves the container running;
//! [`reattach`] re-opens the exec relay over the re-resolved endpoint and
//! [`kill_session`] force-removes it (`session attach/rm` route here on the
//! `remote` field, which works for both registered and inline docker:// URLs).
//! The lifecycle — create → record → list → reattach → teardown-over-endpoint,
//! **with the detached agent staying alive for reattach** — is live-verified
//! against a real VPS.
//!
//! **Version skew (host ↔ runner image).** The host launches the in-container
//! agent via `pillbox run --vault-stdin-direct`; a runner image baked from a
//! pillbox older than that flag rejects it (clap "unexpected argument") and the
//! agent's pty-host child exits on the parse error — which looks like "the
//! agent starts then instantly dies." [`docker::check_runner_protocol_at`]
//! probes for exactly this in preflight and fails loudly with a "rebuild/pull a
//! current image" hint instead. Keep the deployed runner image current with the
//! host pillbox whenever the launch protocol changes.
//!
//! The **drive + read surface** reaches a detached docker:// session like a
//! local one: [`send_input`] drives its pty-host over the endpoint (`session
//! send`), and [`spawn_transcript_stream`] tails the container's transcript out
//! over the endpoint into the host's durable log so `session subscribe`/`watch`
//! read it collector-free (the §0 surface; the sandbox-side OTLP tailer is the
//! with-collector path). docker:// is the one remote whose transcript is
//! host-reachable, since the container is directly `docker exec`-able.
//!
//! Deferred (clearly-marked) follow-ons: host-side **result extraction
//! for a detached session** (`docker cp` out via `session pull`; foreground
//! already pulls on exit), **creds read-back** (the `CredsPersisted`-before-
//! `TornDown` invariant / 2nd-run-401 guard), and OTEL env forwarding.

use std::path::Path;

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
use crate::session::{self, Session, BACKEND_REMOTE_DOCKER};

const ACTION: &str = "run --remote (docker)";

/// Resolve a docker:// remote to its `DOCKER_HOST` endpoint, re-parsing the URL
/// (not caching) so a hand-edited remote surfaces a pointed error. Shared by
/// `run`, `reattach`, and `kill_session` so they agree on how a docker:// remote
/// maps to a daemon — and reject a non-docker URL the same way.
pub(crate) fn endpoint_for(remote: &Remote) -> Result<DockerEndpoint> {
    match remote
        .parsed_url()
        .map_err(|e| PillboxError::config(ACTION, format!("remote `{}`: {e}", remote.name)))?
    {
        RemoteUrl::Docker(d) => Ok(DockerEndpoint::remote(d.docker_host())),
        RemoteUrl::Ssh(_) | RemoteUrl::E2b(_) => Err(PillboxError::config(
            ACTION,
            format!("remote `{}` is not a docker:// URL", remote.name),
        )
        .into()),
    }
}

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
        let endpoint = endpoint_for(&self.remote)?;

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
        // Catch host↔image version skew loudly: an older runner image's pillbox
        // rejects `--vault-stdin-direct` and the agent silently dies. Probe the
        // protocol before building the secret-bearing blob / launching.
        docker::check_runner_protocol_at(&endpoint, &runner_image)?;

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

        // Detached: record the session and leave the container running — no
        // `ContainerGuard`, so it outlives this process. Reattach later over the
        // same endpoint with `pillbox session attach <id>`; tear down with
        // `session rm`. (Result extraction for a detached docker:// session —
        // `docker cp` out over the endpoint via `session pull` — is a follow-on;
        // foreground already pulls on exit below.)
        if opts.detach {
            let session = Session {
                id: session_id.clone(),
                label: opts.label.clone(),
                remote: self.remote.name.clone(),
                backend: BACKEND_REMOTE_DOCKER.to_string(),
                sandbox_id: container.clone(),
                pty_pid: 0,
                agent_id: spec.id.to_string(),
                started_at: session::now_rfc3339(),
                attached_pid: None,
                base_snapshot: None,
                result_snapshot: None,
                expires_at: opts.ttl_seconds.map(session::expires_at_from_ttl),
                // The agent's in-container cwd — keys the transcript scope dir so
                // the read side (`session subscribe`/`watch`) can locate and tail
                // it out of the container over the endpoint. Unlike ssh/e2b (whose
                // transcript is unreachable host-side), a docker:// container is
                // directly `docker exec`-able, so this is host-readable.
                guest_cwd: GUEST_WORKSPACE.to_string(),
            };
            session::write(resolved, &session)?;
            crate::events::emit_session_event(
                resolved,
                crate::events::EventType::SessionStarted {
                    parent_session_id: crate::events::parent_session_id_from_env(),
                },
                &session.id,
                Some(&session),
            );
            if opts.json {
                println!(
                    "{}",
                    crate::paths::json_v1(vec![("session", session.to_json_value())])
                );
            } else {
                println!(
                    "pillbox: ✓ session `{}` started in background on `{}`.",
                    session.id, self.remote.name
                );
                println!("         pillbox session attach {}  # reattach", session.id);
            }
            return Ok(());
        }

        // Foreground: reap the container on every exit path.
        let _guard = ContainerGuard(endpoint.clone(), container.clone());

        let outcome = attach_via_exec_at(&endpoint, &container, false);

        // Result extraction (the SM's `ResultCaptured`): pull the agent's
        // workspace back over the host cwd so the run "feels like local" — your
        // directory reflects what the agent did, like a local bind-mount. Runs
        // on the stopped-but-not-yet-reaped container (docker cp works on a
        // stopped container; the guard reaps only after we return), so it also
        // covers a fast headless agent that exited before the attach connected.
        // Best-effort + loud: a failed pull must not mask the agent's exit code,
        // but the user has to know if their work didn't come back.
        if let Err(e) = docker::cp_out_at(&endpoint, &container, GUEST_WORKSPACE, &workspace_host) {
            eprintln!(
                "pillbox: warning: couldn't pull the remote workspace back to {}: {e}",
                workspace_host.display()
            );
            eprintln!("pillbox: the agent's changes are lost when the container is reaped.");
        }

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
/// Spawn a one-shot endpoint-aware `docker exec … pillbox pty-relay` to the
/// container's pty-host socket — the shared transport for the interactive pump
/// ([`attach_via_exec_at`]) and the one-shot driver ([`send_input`]). Mirrors
/// `local_docker::exec_relay`, but on `endpoint`'s daemon.
fn exec_relay_at(endpoint: &DockerEndpoint, container: &str) -> Result<std::process::Child> {
    docker::exec_attach_at(
        endpoint,
        container,
        &[
            "pillbox".into(),
            "pty-relay".into(),
            "--sock".into(),
            ATTACH_SOCK.into(),
        ],
    )
}

fn attach_via_exec_at(
    endpoint: &DockerEndpoint,
    container: &str,
    detach_enabled: bool,
) -> Result<Outcome> {
    let mut child = exec_relay_at(endpoint, container)?;
    let stdout = child.stdout.take().context("docker exec relay stdout")?;
    let stdin = child.stdin.take().context("docker exec relay stdin")?;
    let outcome = pump::attach_terminal(stdout, stdin, detach_enabled)?;
    let _ = child.kill();
    let _ = child.wait();
    Ok(outcome)
}

/// Push one `Input` frame to a running docker:// session's pty-host over the
/// endpoint — the `SendInput` half of the drive surface (`pillbox session
/// send`). Mirrors `local_docker::send_input` but endpoint-aware: the relay
/// exec rides `DOCKER_HOST=ssh://…` to the remote daemon, then the shared
/// frame/EOF protocol ([`crate::attach::driver::drive_once`]) drives it.
pub(crate) fn send_input(endpoint: &DockerEndpoint, container: &str, bytes: &[u8]) -> Result<()> {
    crate::attach::driver::drive_once(exec_relay_at(endpoint, container)?, bytes)
        .context("drive the session's pty-relay")
}

/// Stream a detached docker:// session's transcript out of the container into
/// the host's durable [`SessionLog`] — the *read* half of the drive surface, so
/// `session subscribe`/`watch` work on a remote session exactly like a local
/// one, **collector-free** (the §0 surface; the sandbox-side OTLP tailer is the
/// with-collector path). The container's transcript is unreachable as a file,
/// but a docker:// container is directly `docker exec`-able, so we tail it over
/// the endpoint and feed the bytes through the same [`Tailer`] the bind-mounted
/// local path uses.
///
/// The transcript uuid isn't known ahead of time, so the in-container shell
/// waits for the newest `*.jsonl` under the harness scope dir, then `tail -F`s
/// it; the host thread parses that stream into `log`. Returns `None` for an
/// agent with no transcript parser (the caller then just reads the existing
/// log). Held by a [`TailerHandle`] for the stream's lifetime.
pub(crate) fn spawn_transcript_stream(
    endpoint: &DockerEndpoint,
    container: &str,
    agent_id: &str,
    guest_cwd: &str,
    session_id: &str,
    log: crate::events::log::SessionLog,
) -> Option<crate::events::transcripts::TailerHandle> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use crate::events::transcripts::{Harness, Tailer};

    let harness = Harness::for_agent(agent_id)?;
    // Container-side transcript root, RELATIVE to the agent's `$HOME` — the
    // runner's run user is `/home/pillbox`, not `/root`, and a future image
    // could change it, so we resolve `$HOME` in the container shell rather than
    // hardcode a path. The relative segment (`.claude/projects/<scope>` for
    // Claude; `.codex/sessions` for Codex) is dash-encoded → safe unquoted.
    let (watch_root, scope_dir) = harness.transcript_roots(Path::new(""), guest_cwd);
    let rel = scope_dir.unwrap_or(watch_root);
    let script = format!(
        "root=\"$HOME/{rel}\"; while :; do f=$(find \"$root\" -name '*.jsonl' -type f \
         -printf '%T@ %p\\n' 2>/dev/null | sort -rn | head -n1 | cut -d' ' -f2-); \
         [ -n \"$f\" ] && break; sleep 0.3; done; exec tail -n +1 -F \"$f\"",
        rel = rel.to_string_lossy(),
    );

    let mut child =
        docker::exec_attach_at(endpoint, container, &["sh".into(), "-c".into(), script])
            .map_err(|e| eprintln!("pillbox: warning: couldn't tail the remote transcript: {e:#}"))
            .ok()?;
    let stdout = child.stdout.take()?;

    // The handle owns the child so stopping the stream can kill the exec (which
    // EOFs the pipe and unblocks the thread's parked read); the thread only
    // reads stdout + observes `stop` between reads.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let sid = session_id.to_string();
    let join = std::thread::spawn(move || {
        let mut tailer = Tailer::for_stream(sid, harness, true, Some(log));
        if let Err(e) = tailer.follow_reader(stdout, &stop_thread) {
            eprintln!("pillbox: warning: remote transcript stream stopped: {e:#}");
        }
    });
    Some(crate::events::transcripts::TailerHandle::from_stream(
        stop, child, join,
    ))
}

/// `pillbox session attach <id>` for a docker:// session: re-open the
/// docker-exec relay to the still-running remote container and pump. Mirrors
/// `local_docker::reattach` / `remote_ssh::reattach`, but endpoint-aware.
pub(crate) fn reattach(resolved: &Pillbox, remote: &Remote, session: &Session) -> Result<()> {
    if session::Backend::parse(&session.backend) != Some(session::Backend::RemoteDocker) {
        return Err(PillboxError::usage(
            "session attach",
            format!(
                "session `{}` is backed by `{}`, not a remote docker",
                session.id, session.backend
            ),
        )
        .into());
    }
    let endpoint = endpoint_for(remote)?;
    let short = &session.sandbox_id[..session.sandbox_id.len().min(12)];
    eprintln!(
        "pillbox: reattaching to session `{}` (container `{short}`) on `{}` …",
        session.id, remote.name
    );
    eprintln!("pillbox: detach with Ctrl-A D (the container keeps running).");

    session::mark_attached(resolved, &session.id, std::process::id() as i64)?;
    let outcome = attach_via_exec_at(&endpoint, &session.sandbox_id, true);
    let _ = session::mark_detached(resolved, &session.id);

    match outcome? {
        // Clean detach (Ctrl-A D) or a dropped transport — either way the
        // container keeps running and the record is left in place, so the
        // session is still reattachable. Tell the user how.
        Outcome::Detached | Outcome::Disconnected => {
            eprintln!(
                "pillbox: detached. reattach with `pillbox session attach {}`",
                session.id
            );
            Ok(())
        }
        Outcome::Exited(code) => {
            eprintln!(
                "pillbox: agent exited ({code}). `pillbox session rm {}` to clean up.",
                session.id
            );
            Ok(())
        }
    }
}

/// `pillbox session rm <id>` for a docker:// session: force-remove the remote
/// container over the endpoint, then drop the local record unconditionally (a
/// failed remove must not strand the record; the container may already be
/// gone). `remote` is `None` when it's been deregistered — we can't reach the
/// daemon to remove the container, but we still drop the local record.
pub(crate) fn kill_session(
    resolved: &Pillbox,
    remote: Option<&Remote>,
    session: &Session,
) -> Result<()> {
    if session::Backend::parse(&session.backend) != Some(session::Backend::RemoteDocker) {
        return Err(PillboxError::usage(
            "session rm",
            format!(
                "session `{}` is backed by `{}`, not a remote docker",
                session.id, session.backend
            ),
        )
        .into());
    }
    match remote {
        Some(remote) => match endpoint_for(remote) {
            Ok(endpoint) => {
                if let Err(e) = docker::rm_force_at(&endpoint, &session.sandbox_id) {
                    eprintln!("pillbox: warning: remote container teardown failed: {e}");
                }
            }
            Err(e) => eprintln!(
                "pillbox: warning: remote `{}` url unusable ({e}); skipping remote teardown.",
                remote.name
            ),
        },
        None => eprintln!(
            "pillbox: warning: remote `{}` is no longer registered — dropping the record without \
             remote teardown; remove the container by hand if it's still running.",
            session.remote
        ),
    }
    crate::events::emit_session_event(
        resolved,
        crate::events::EventType::SessionDropped,
        &session.id,
        Some(session),
    );
    session::delete(resolved, &session.id)?;
    println!("pillbox: ✓ session `{}` removed.", session.id);
    Ok(())
}
