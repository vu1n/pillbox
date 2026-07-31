//! `SandboxBackend` implementation that uses the host's Docker daemon.
//!
//! v0.6: takes a resolved [`Pillbox`] so the agent's auth home + vault
//! state come from the right scope. Auth currently always resolves to
//! global; vault state lives per-pillbox so a project's leases never
//! collide with another's.
// Context: doc://pillbox/adr-002-docker-backend-deleted@0001#docker-backend-deleted

use std::time::SystemTime;

use anyhow::{Context, Result};

use super::{Caps, SandboxBackend};
use crate::agents::{
    base_docker_args_detached, resolve_run_env, resolve_with_entries, workspace_mount_name,
    AgentSpec, Integration, RunOpts, GUEST_HOME, GUEST_WORKSPACE,
};
use crate::attach::pump::{self, Outcome};
use crate::pillbox::Pillbox;
use crate::session::{self, Session, BACKEND_DOCKER};
use crate::startup::StartupTimer;
use crate::workspace::WorkspaceBackend;
use crate::{docker, errors::PillboxError};

pub(crate) struct DockerBackend;

/// Where the in-container pty-host listens; the per-attach relay (run via
/// `docker exec`) connects to the same path. One pty-host per container.
const ATTACH_SOCK: &str = "/tmp/pillbox-attach.sock";

/// Force-removes its container on drop. Used only on the foreground run
/// path so the `-d` container is torn down on every normal/early-return/
/// panic exit (a detached session deliberately outlives the CLI and is
/// reaped by `session rm` instead).
struct ContainerGuard(String);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = docker::rm_force(&self.0);
    }
}

impl SandboxBackend for DockerBackend {
    /// The container family: full PTY drive/read + long-lived exec, opencode
    /// server mode. No KVM-isolation features (real fence, in-sandbox grade,
    /// detached vault); drains §0 live, so no post-hoc ingest. See
    /// docs/substrate-plane.md.
    fn capabilities(&self) -> Caps {
        Caps {
            pty_drive: true,
            live_pty_tail: true,
            server_mode: true,
            long_lived_exec: true,
            in_sandbox_grading: false,
            real_egress_fence: false,
            detached_vault: false,
            post_hoc_ingest: false,
        }
    }

    fn id(&self) -> &'static str {
        BACKEND_DOCKER
    }

    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        if spec.integration == Integration::Structured {
            return Err(PillboxError::usage(
                "run",
                format!(
                    "`{}` structured stdout runs on the libkrun backend only (build --features libkrun)",
                    spec.id
                ),
            )
            .with_next("unset PILLBOX_BACKEND or set PILLBOX_BACKEND=libkrun")
            .into());
        }
        // Some server agents (codex-serve) only run on the libkrun backend — their
        // run path lives in the microVM. Reject on docker rather than mis-routing
        // through the opencode server path below. Keyed on the capability, not the
        // id, so a future libkrun-only server agent is covered without a new branch.
        if spec.server.is_some_and(|p| p.libkrun_only) {
            return Err(PillboxError::usage(
                "run",
                format!(
                    "`{}` runs on the libkrun backend only (build --features libkrun)",
                    spec.id
                ),
            )
            .with_next("pillbox run --agent codex   # the docker-capable PTY codex")
            .into());
        }
        // Server-integration agents (opencode) run headless + are driven/read
        // over their HTTP API — a distinct path with no PTY. Keep it off the
        // PTY path entirely so claude/codex are untouched.
        if spec.integration == Integration::Server {
            return run_server(spec, opts, resolved);
        }
        let mut startup = StartupTimer::start();
        let runner_image = docker::check_ready_for(resolved)?;
        startup.mark("docker_preflight");

        let home = spec.home_dir(resolved)?;
        if !home.join(spec.cred_sentinel).exists() {
            return Err(PillboxError::runtime(
                "run",
                format!("no stored credentials for `{}`", spec.id),
            )
            .with_next(format!("pillbox auth login --agent {}", spec.id))
            .into());
        }
        startup.mark("auth_check");

        let workspace_host = match &opts.workspace {
            Some(p) => p.clone(),
            None => std::env::current_dir().context("resolve current working directory")?,
        };
        if let Some(name) = opts.from_bookmark.as_deref() {
            let handle = crate::bookmarks::resolve_existing(resolved, name)?;
            resolved.workspace()?.pull(&workspace_host, Some(&handle))?;
        }
        let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
        let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");
        // Kept for the transcript tailer's scope-dir derivation; the
        // original is moved into the docker args below.
        let guest_cwd = guest_workspace.clone();
        startup.mark("workspace_prepare");

        if opts.vault && !spec.vault_capable {
            return Err(PillboxError::usage(
                "run",
                format!("--vault is not supported for `{}`", spec.id),
            )
            .into());
        }

        let withs_resolved = resolve_with_entries(resolved, &opts.withs)?;
        let any_vaulted = withs_resolved.iter().any(|w| w.meta.is_some());
        // A vaulted `--with` secret routes through the stub-swap proxy
        // exactly like `--vault`; an agent that can't reach the proxy
        // would receive the stub and ship it to the provider. Reject
        // here too, not just on the explicit `--vault` flag above.
        if any_vaulted && !spec.vault_capable {
            let names: Vec<&str> = withs_resolved
                .iter()
                .filter(|w| w.meta.is_some())
                .map(|w| w.secret_name.as_str())
                .collect();
            return Err(PillboxError::usage(
                "run",
                format!(
                    "agent `{}` does not support the vault proxy, so it can't use vaulted secret(s): {}",
                    spec.id,
                    names.join(", ")
                ),
            )
            .into());
        }
        // Local --detach can't use the vault: the stub-swap proxy runs on the
        // host and would die the moment this CLI returns, leaving the detached
        // agent with dead credentials. Sourced from the capability so the gate
        // tracks the backend's real detached_vault support, not a hardcode.
        if opts.detach && (opts.vault || any_vaulted) && !self.capabilities().detached_vault {
            return Err(PillboxError::usage(
                "run",
                "--detach does not support the vault locally (the proxy can't outlive the CLI)",
            )
            .with_next("run without --vault, or use the default libkrun backend (don't set PILLBOX_BACKEND=docker), which keeps the vault on detach")
            .into());
        }
        // One session id for the whole foreground run: it anchors the
        // OTLP trace (root session span below), parents the vault MITM
        // gen_ai spans, and parents the host-side transcript tailer's
        // thread spans — all three correlate by deriving from it. The
        // detach branch returns before reaching the span/tailer wiring
        // and mints its own record id, so this is unused there.
        let session_id = Session::new_id();

        let mut vault_session = if opts.vault || any_vaulted {
            let oauth = if opts.vault {
                Some(crate::vault::OAuthAgent {
                    agent_id: spec.id,
                    agent_home: &home,
                })
            } else {
                None
            };
            // Thread the session id through so gen_ai spans nest under
            // the host-emitted session span instead of rooting per
            // lease. Mode + workspace_id let eval consumers group
            // traces by project / attentiveness regime.
            let context = crate::vault::RunContext {
                session_id: Some(session_id.clone()),
                mode: Some("interactive".to_string()),
                workspace_id: Some(resolved.workspace_id().to_string()),
            };
            let egress = crate::vault::EgressPolicy {
                default_deny: opts.egress_deny,
                allow_hosts: opts.egress_allow.clone(),
            };
            if opts.egress_deny {
                // Honest about the boundary: docker's network can't be cleanly
                // egress-fenced (Docker Desktop runs containers in a LinuxKit VM
                // with no reachable host iptables), so default-deny here is
                // proxy-level only — it constrains proxy-honoring clients
                // (claude/codex/node) but a client that ignores HTTPS_PROXY can
                // still dial direct. libkrun owns its egress stack and fences at
                // DNS, so it's the airtight path. See docs/vault.md.
                eprintln!(
                    "pillbox: note: --egress-deny on docker is proxy-level only — it does \
                     not network-fence direct dials. For sole-egress, use the default libkrun \
                     backend (don't set PILLBOX_BACKEND=docker)."
                );
            }
            Some(crate::vault::VaultSession::start(
                oauth, resolved, context, egress,
            )?)
        } else {
            if opts.egress_deny {
                eprintln!(
                    "pillbox: note: --egress-deny has no effect without --vault \
                     (or a vaulted --with) — the proxy that enforces it isn't running."
                );
            }
            None
        };

        let env_vars = resolve_run_env(resolved, &opts, &withs_resolved, vault_session.as_mut())?;
        startup.mark("env_prepare");

        // Bind `mcp` (rather than letting the expression be the
        // tail of an if-let in the args build) so the tempfile
        // inside `McpInjection` lives until docker exits.
        let mcp = if opts.mcps.is_empty() {
            if !opts.mcp_tokens.is_empty() {
                return Err(PillboxError::usage(
                    "run",
                    "--mcp-token requires at least one --mcp NAME=URL",
                )
                .into());
            }
            None
        } else {
            let inject = spec.mcp_inject.ok_or_else(|| {
                PillboxError::usage(
                    "run",
                    format!("--mcp is not supported for agent `{}`", spec.id),
                )
            })?;
            let resolved_mcps =
                crate::agents::mcp::resolve_tokens(resolved, opts.mcps.clone(), &opts.mcp_tokens)?;
            Some(inject(&resolved_mcps)?)
        };

        // Detached container so the pty-host outlives the client; we attach
        // over a docker-exec relay and tear it down explicitly.
        let mut args = base_docker_args_detached();
        args.extend([
            "-v".into(),
            format!("{}:{GUEST_HOME}", home.display()),
            "-v".into(),
            format!("{}:{guest_workspace}", workspace_host.display()),
            "-w".into(),
            guest_workspace,
        ]);
        for m in &opts.mounts {
            args.push("-v".into());
            args.push(m.clone());
        }
        if let Some(mount) = mcp.as_ref().and_then(|m| m.docker_mount.as_ref()) {
            args.push("-v".into());
            args.push(mount.clone());
        }
        // Secret env reaches the container by NAME only (`-e KEY`), its value set on
        // the docker client's environment in `run_detached` — never `-e KEY=VALUE` in
        // argv, which `ps`/`/proc/<pid>/cmdline` expose to other local uids.
        let mut secret_env = env_vars.clone();
        if let Some(mcp) = &mcp {
            secret_env.extend(mcp.env_vars.iter().cloned());
        }
        docker::push_secret_env_flags(&mut args, &secret_env);
        if let Some(session) = &vault_session {
            args.extend(session.docker_extras(GUEST_HOME));
            eprintln!(
                "pillbox: vault proxy listening on {} (ca: {})",
                session.listen_addr(),
                session.ca_cert_path().display()
            );
        }
        args.push(runner_image);
        // Run the agent under the in-sandbox pty-host instead of directly,
        // so the same frame protocol carries it as the remote backends.
        args.extend([
            "pillbox".into(),
            "pty-host".into(),
            "--sock".into(),
            ATTACH_SOCK.into(),
            "--".into(),
        ]);
        args.extend(spec.run_argv.iter().map(|s| s.to_string()));
        if let Some(mcp) = &mcp {
            args.extend(mcp.extra_argv.iter().cloned());
        }
        // Sandbox defaults (e.g. claude `--permission-mode auto`) before the
        // user's `-- args`, so the user can still override.
        args.extend(spec.sandbox_args.iter().map(|s| s.to_string()));
        args.extend(opts.args);

        // Pre-accept the agent's workspace trust dialog (claude) on the
        // bind-mounted home before the container starts, so an interactive run
        // doesn't stall on the gate (claude runs with cwd `guest_cwd`).
        spec.prepare_workspace_or_warn(&home, &guest_cwd);
        startup.mark("agent_prepare");

        let run_started = SystemTime::now();
        let container = docker::run_detached(&args, &secret_env)?;
        startup.mark("container_start");

        if opts.detach {
            // vault was rejected above, so there's no host-side proxy to keep
            // alive: record the session and return. Reattach later with the
            // same docker-exec relay via `pillbox session attach <id>`.
            let session = Session {
                id: Session::new_id(),
                label: opts.label.clone(),
                backend: BACKEND_DOCKER.to_string(),
                sandbox_id: container,
                pty_pid: 0,
                agent_id: spec.id.to_string(),
                started_at: session::now_rfc3339(),
                attached_pid: None,
                base_snapshot: None,
                result_snapshot: None,
                expires_at: opts.ttl_seconds.map(session::expires_at_from_ttl),
                guest_cwd: guest_cwd.clone(),
                placement: session::Placement::Local,
                server: None,
                requested_execution: None,
            };
            session::write(resolved, &session)?;
            let startup_metrics = startup.finish("session_record");
            crate::events::emit_session_event(
                resolved,
                crate::events::EventType::SessionStarted {
                    parent_session_id: crate::events::parent_session_id_from_env(),
                    startup: Some(startup_metrics),
                },
                &session.id,
                Some(&session),
            );
            if opts.json {
                session::print_started_json(&session);
            } else {
                let short = &session.sandbox_id[..session.sandbox_id.len().min(12)];
                println!(
                    "pillbox: ✓ session `{}` started in background (container `{short}`).",
                    session.id
                );
                println!("         pillbox session attach {}  # reattach", session.id);
            }
            return Ok(());
        }

        // Foreground run: rm the container on every exit path — including an
        // early `?`-return from attach or a panic — via a drop guard, not a
        // single explicit call a mid-run failure could skip. (An external
        // SIGKILL still bypasses Drop; that residual orphan is the lifecycle
        // follow-up, alongside the ssh remote-container reap.)
        let _container = ContainerGuard(container.clone());
        let started_session = Session {
            id: session_id.clone(),
            label: None,
            backend: BACKEND_DOCKER.to_string(),
            sandbox_id: container.clone(),
            pty_pid: 0,
            agent_id: spec.id.to_string(),
            started_at: session::now_rfc3339(),
            attached_pid: Some(std::process::id() as i64),
            base_snapshot: None,
            result_snapshot: None,
            expires_at: None,
            guest_cwd: guest_cwd.clone(),
            placement: session::Placement::Local,
            server: None,
            requested_execution: None,
        };
        let startup_metrics = startup.finish("host_started_event");
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: Some(startup_metrics),
            },
            &started_session.id,
            Some(&started_session),
        );

        // Open the root `session` span up-front + start live transcript
        // streaming. The agent's `$HOME` is bind-mounted from the host
        // (above), so its `~/.claude/projects/<uuid>.jsonl` lands on a
        // host path the tailer reads straight into the operator's
        // collector — no egress hop. The session span must precede the
        // children (a collector names a run from the first span it sees).
        // Shared with the remote backends, which run the same bootstrap
        // sandbox-side. See spawn_session_observability for the
        // include_usage / MITM-double-count reasoning.
        let proxy_active = opts.vault || any_vaulted;
        // Open the durable per-session §0 sink (the spine's first producer)
        // through the placement swap point: the local file-backed log by default,
        // or the managed Durable Object when the managed tier is on (env-gated in
        // `open_event_log`). The agent's $HOME is host-bind-mounted, so the
        // transcript is always written host-side; the default placement tails it
        // into ~/.pillbox's log.jsonl, the managed placement streams it to the DO.
        // Best-effort (`open_or_warn`): a sink-open failure falls back to OTLP-only
        // rather than aborting the run.
        let log = crate::events::sink::open_or_warn(resolved, &session_id);
        let tailer = crate::events::transcripts::spawn_session_observability(
            log,
            &session_id,
            spec.id,
            &home,
            &guest_cwd,
            proxy_active,
            run_started,
        );

        let outcome = attach_via_exec(&container, false);
        // The vault proxy must stay up for the whole attached session.
        drop(vault_session);
        // Stop the tailer with a final drain of the agent's last lines.
        if let Some(tailer) = tailer {
            tailer.shutdown();
        }

        match outcome? {
            Outcome::Exited(0) | Outcome::Detached | Outcome::Disconnected => Ok(()),
            Outcome::Exited(code) => Err(PillboxError::runtime(
                "run",
                format!("{} exited with status {code}", spec.id),
            )
            .into()),
        }
    }
}

/// Run a `Server`-integration agent (opencode) on the local daemon: launch
/// `opencode serve` headless (no pty-host, no vault — opencode isn't
/// vault-capable), record a session keyed to the opencode session id, optionally
/// send an initial prompt, and return. There's no PTY to attach; the user reads
/// with `session watch`/`subscribe` (which spawn the event bridge) and drives
/// with `session send`. Always "background server" — `--detach` is implicit.
fn run_server(spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
    let action = "run";
    let mut startup = StartupTimer::start();
    let runner_image = docker::check_ready_for(resolved)?;
    startup.mark("docker_preflight");

    let home = spec.home_dir(resolved)?;
    if !home.join(spec.cred_sentinel).exists() {
        return Err(PillboxError::runtime(
            action,
            format!("no stored credentials for `{}`", spec.id),
        )
        .with_next(format!("pillbox auth login --agent {}", spec.id))
        .into());
    }
    startup.mark("auth_check");
    // opencode has no vault integration (`vault_capable: false`); refuse rather
    // than silently hand the agent a stub it would ship to the provider.
    if opts.vault {
        return Err(PillboxError::usage(
            action,
            format!("--vault is not supported for `{}`", spec.id),
        )
        .into());
    }
    let withs_resolved = resolve_with_entries(resolved, &opts.withs)?;
    if withs_resolved.iter().any(|w| w.meta.is_some()) {
        return Err(PillboxError::usage(
            action,
            format!("vaulted secrets are not supported for `{}`", spec.id),
        )
        .into());
    }

    let workspace_host = match &opts.workspace {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("resolve current working directory")?,
    };
    if let Some(name) = opts.from_bookmark.as_deref() {
        let handle = crate::bookmarks::resolve_existing(resolved, name)?;
        resolved.workspace()?.pull(&workspace_host, Some(&handle))?;
    }
    let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
    let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");
    let env_vars = resolve_run_env(resolved, &opts, &withs_resolved, None)?;
    startup.mark("workspace_env_prepare");

    // Detached container (no `-d` reap guard — the server outlives the CLI and
    // is reaped by `session rm`). No pty-host: the command IS `opencode serve`.
    let mut args = base_docker_args_detached();
    args.extend([
        "-v".into(),
        format!("{}:{GUEST_HOME}", home.display()),
        "-v".into(),
        format!("{}:{guest_workspace}", workspace_host.display()),
        "-w".into(),
        guest_workspace.clone(),
    ]);
    for m in &opts.mounts {
        args.push("-v".into());
        args.push(m.clone());
    }
    // Secret env by name only (value via the Command env in run_detached) — not
    // `-e KEY=VALUE` argv, which other local uids can read via `ps`.
    docker::push_secret_env_flags(&mut args, &env_vars);
    args.push(runner_image);
    args.extend(super::opencode::serve_args());

    let container = docker::run_detached(&args, &env_vars)?;
    startup.mark("container_start");
    let http = super::http::DockerHttp::new(container.clone(), super::opencode::SERVE_PORT);
    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| super::opencode::DEFAULT_MODEL.to_string());
    let prompt = opts.args.join(" ").trim().to_string();

    // Everything after launch can fail; reap the container if it does so a
    // failed bring-up doesn't leak a server.
    let built = (|| -> Result<Session> {
        super::opencode::wait_ready(&http)?;
        let ocid = super::opencode::create_session(&http)?;
        startup.mark("server_ready");
        let session = Session {
            id: Session::new_id(),
            label: opts.label.clone(),
            backend: BACKEND_DOCKER.to_string(),
            sandbox_id: container.clone(),
            pty_pid: 0,
            agent_id: spec.id.to_string(),
            started_at: session::now_rfc3339(),
            attached_pid: None,
            base_snapshot: None,
            result_snapshot: None,
            expires_at: opts.ttl_seconds.map(session::expires_at_from_ttl),
            guest_cwd: guest_workspace.clone(),
            placement: session::Placement::Local,
            server: Some(session::ServerSession {
                agent_session_id: ocid.clone(),
                model: model.clone(),
                temperature: opts.temperature,
            }),
            requested_execution: Some(opts.requested_profile(&model)?),
        };
        session::write(resolved, &session)?;
        let startup_metrics = startup.finish("session_record");
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionStarted {
                parent_session_id: crate::events::parent_session_id_from_env(),
                startup: Some(startup_metrics),
            },
            &session.id,
            Some(&session),
        );
        Ok(session)
    })();
    let session = match built {
        Ok(s) => s,
        Err(e) => {
            let _ = docker::rm_force(&container);
            return Err(e);
        }
    };

    // No auto-send: opencode comes up ready (wait_ready), so the first prompt
    // goes through `session send` like every other — captured by a subscribed
    // watch instead of streamed to no one at start.
    super::opencode::print_started(
        &session,
        opts.json,
        (!prompt.is_empty()).then_some(prompt.as_str()),
    );
    Ok(())
}

/// Attach the terminal pump to a running pty-host container by execing the
/// per-attach relay and pumping over its stdio. `detach_enabled` is false
/// for a foreground `run` (no session to leave behind, so Ctrl-A passes
/// through and SIGTERM terminates) and true for `session attach`.
/// Spawn a one-shot docker-exec `pty-relay` to a container's pty-host socket —
/// the shared transport for the interactive pump ([`attach_via_exec`]) and the
/// one-shot driver ([`send_input`]). The caller takes stdin/stdout off the child.
fn exec_relay(container: &str) -> Result<std::process::Child> {
    docker::exec_attach(
        container,
        &[
            "pillbox".into(),
            "pty-relay".into(),
            "--sock".into(),
            ATTACH_SOCK.into(),
        ],
    )
}

fn attach_via_exec(container: &str, detach_enabled: bool) -> Result<Outcome> {
    let mut child = exec_relay(container)?;
    let stdout = child.stdout.take().context("docker exec relay stdout")?;
    let stdin = child.stdin.take().context("docker exec relay stdin")?;
    let outcome = pump::attach_terminal(stdout, stdin, detach_enabled)?;
    // Don't leave the relay exec lingering.
    let _ = child.kill();
    let _ = child.wait();
    Ok(outcome)
}

/// Push one `Input` frame to a running session's in-container pty-host via a
/// one-shot docker-exec relay — the same transport `attach` uses, but driven
/// programmatically (no pump). The `SendInput` half of the drive surface: bytes
/// in, as if typed. `pillbox session send <id>`. The relay-spawn is the only
/// docker-specific bit; the frame/EOF/bounded-wait protocol lives in
/// [`crate::attach::driver::drive_once`] (shared with the docker:// backend).
pub(crate) fn send_input(container: &str, bytes: &[u8]) -> Result<()> {
    crate::attach::driver::drive_once(exec_relay(container)?, bytes)
        .context("drive the session's pty-relay")
}

/// `pillbox session attach <id>` for a local Docker session: re-open the
/// docker-exec relay to the still-running pty-host container and pump.
pub(crate) fn reattach(resolved: &Pillbox, session: &Session) -> Result<()> {
    let short = &session.sandbox_id[..session.sandbox_id.len().min(12)];
    eprintln!(
        "pillbox: reattaching to session `{}` (container `{short}`) …",
        session.id
    );
    eprintln!("pillbox: detach with Ctrl-A D (the container keeps running).");

    session::mark_attached(resolved, &session.id, std::process::id() as i64)?;
    let outcome = attach_via_exec(&session.sandbox_id, true);
    // Always clear the attached stamp; the record is still valid.
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
            // The agent exited, so the pty-host (container PID 1) stopped too.
            // Leave the record for `session rm` / inspection.
            eprintln!(
                "pillbox: agent exited ({code}). `pillbox session rm {}` to clean up.",
                session.id
            );
            Ok(())
        }
    }
}

/// `pillbox session rm <id>` for a local Docker session: force-remove the
/// container, then drop the record (unconditionally — a failed remove
/// shouldn't strand the record; the container may already be gone).
pub(crate) fn kill_session(resolved: &Pillbox, session: &Session) -> Result<()> {
    let _ = docker::rm_force(&session.sandbox_id);
    // Emit the lifecycle event before deleting so the payload can reference a
    // still-valid record — parity with the e2b/ssh backends (orchestrators
    // tailing the events stream must see docker teardowns too).
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

/// The docker [`LiveSession`] — a running container (foreground or detached)
/// the command layer drives and reads without branching on the backend. A thin
/// adapter: every verb forwards to the existing free fn (the proven transport),
/// so the plane gains no new docker behavior, only one polymorphic surface.
/// Holds a cloned [`Session`] record (the container id is its `sandbox_id`).
pub(crate) struct DockerLiveSession {
    session: Session,
}

impl super::LiveSession for DockerLiveSession {
    fn caps(&self) -> Caps {
        DockerBackend.capabilities()
    }

    fn send(&self, bytes: &[u8]) -> Result<()> {
        // A server agent's turn is a structured prompt over its HTTP API; a PTY
        // agent's is raw keystrokes. Both flow through this one `send` so the
        // command layer never branches on integration.
        if self.session.integration() == crate::agents::Integration::Server {
            return super::drive_server_prompt(&self.session, &*self.http()?, bytes);
        }
        send_input(&self.session.sandbox_id, bytes)
    }

    fn attach(&self, resolved: &Pillbox) -> Result<()> {
        reattach(resolved, &self.session)
    }

    fn spawn_log_tailer(
        &self,
        resolved: &Pillbox,
    ) -> Result<Option<crate::events::transcripts::TailerHandle>> {
        let log = crate::events::log::SessionLog::open(resolved, &self.session.id)?;
        // A server-mode agent (opencode) has no transcript file — bridge its HTTP
        // `/event` stream into the log; a PTY agent's transcript lands on the
        // bind-mounted host home, so tail that. Both fill the durable log the
        // caller then opens to read (it owns the placement swap: file vs DO).
        let tailer = if self.session.integration() == Integration::Server {
            let http = self.docker_http()?;
            super::opencode::spawn_event_bridge(&http, &self.session.id, log)
        } else {
            let spec = crate::agents::lookup("session", &self.session.agent_id)?;
            let home = spec.home_dir(resolved)?;
            crate::events::transcripts::spawn_attach_tailer(
                log,
                &home,
                &self.session.agent_id,
                &self.session.guest_cwd,
                &self.session.id,
            )
        };
        Ok(tailer)
    }

    fn http(&self) -> Result<Box<dyn crate::sandbox::http::SandboxHttp>> {
        // Only a server-mode agent runs an in-sandbox HTTP server to talk to; a
        // PTY agent has none, so the verb is unsupported rather than handing back
        // a handle that would `curl` a closed port.
        if self.session.integration() != Integration::Server {
            return Err(self.caps().unsupported("http"));
        }
        Ok(Box::new(self.docker_http()?))
    }

    fn workspace_path(&self) -> Result<std::path::PathBuf> {
        // Docker bind-mounts the live host workspace (no CoW result clone), and the
        // record persists only the *guest* mount path (`guest_cwd`), never the host
        // source. With no recoverable host result-workspace, the verb is unsupported
        // — matching `caps().in_sandbox_grading == false`.
        Err(self.caps().unsupported("workspace_path"))
    }

    fn ingest(&self, _resolved: &Pillbox) -> Result<usize> {
        // Docker drains §0 live via the attach tailer / event bridge — there's no
        // headless capture file to drain post-hoc.
        Err(self.caps().unsupported("ingest"))
    }

    fn kill(&self, resolved: &Pillbox) -> Result<()> {
        kill_session(resolved, &self.session)
    }
}

impl DockerLiveSession {
    pub(crate) fn new(session: Session) -> Self {
        Self { session }
    }

    /// The `docker exec curl` HTTP transport to this session's in-container
    /// server — shared by [`event_source`](Self::event_source) (the opencode
    /// bridge) and [`http`](Self::http). Mirrors the server bring-up in
    /// `commands::session` so the dispatch sites converge on one construction.
    fn docker_http(&self) -> Result<super::http::DockerHttp> {
        Ok(super::http::DockerHttp::new(
            self.session.sandbox_id.clone(),
            super::opencode::SERVE_PORT,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::sandbox::LiveSession;
    use crate::test_util::with_isolated_home;

    #[test]
    fn live_session_reports_pty_caps_and_rejects_ingest() {
        let live = DockerLiveSession::new(Session::test_fixture());
        assert!(
            live.caps().pty_drive,
            "docker is the full-PTY family — pty_drive must be true"
        );
        with_isolated_home("docker-livesession-ingest", || {
            let err = live.ingest(&pillbox::global()).unwrap_err().to_string();
            assert!(
                err.contains("ingest"),
                "the unsupported error must name the rejected verb, got: {err}"
            );
        });
    }
}
