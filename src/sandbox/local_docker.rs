//! `SandboxBackend` implementation that uses the host's Docker daemon.
//!
//! v0.6: takes a resolved [`Pillbox`] so the agent's auth home + vault
//! state come from the right scope. Auth currently always resolves to
//! global; vault state lives per-pillbox so a project's leases never
//! collide with another's.

use std::time::SystemTime;

use anyhow::{Context, Result};

use super::SandboxBackend;
use crate::agents::{
    base_docker_args_detached, resolve_run_env, resolve_with_entries, workspace_mount_name,
    AgentSpec, RunOpts, GUEST_HOME, GUEST_WORKSPACE,
};
use crate::attach::pump::{self, Outcome};
use crate::pillbox::Pillbox;
use crate::session::{self, Session, BACKEND_DOCKER};
use crate::workspace::WorkspaceBackend;
use crate::{docker, errors::PillboxError};

pub(crate) struct LocalDocker;

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

impl SandboxBackend for LocalDocker {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        let runner_image = docker::check_ready_for(resolved)?;

        let home = spec.home_dir(resolved)?;
        if !home.join(spec.cred_sentinel).exists() {
            return Err(PillboxError::runtime(
                "run",
                format!("no stored credentials for `{}`", spec.id),
            )
            .with_next(format!("pillbox auth login --agent {}", spec.id))
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
        // Kept for the transcript tailer's scope-dir derivation; the
        // original is moved into the docker args below.
        let guest_cwd = guest_workspace.clone();

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
        // agent with dead credentials. Reject early with a pointer.
        if opts.detach && (opts.vault || any_vaulted) {
            return Err(PillboxError::usage(
                "run",
                "--detach does not support the vault locally (the proxy can't outlive the CLI)",
            )
            .with_next("run without --vault, or use --remote for a vaulted detached session")
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
            Some(crate::vault::VaultSession::start(oauth, resolved, context)?)
        } else {
            None
        };

        let env_vars = resolve_run_env(resolved, &opts, &withs_resolved, vault_session.as_mut())?;

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
        for (k, v) in &env_vars {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        if let Some(mcp) = &mcp {
            for (k, v) in &mcp.env_vars {
                args.push("-e".into());
                args.push(format!("{k}={v}"));
            }
        }
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
        // doesn't stall on the gate. Best-effort + loud: a prep failure
        // shouldn't abort the run (the dialog just reappears in-session).
        if let Some(prepare) = spec.prepare_workspace {
            if let Err(e) = prepare(&home, &guest_cwd) {
                eprintln!("pillbox: warning: workspace pre-trust failed: {e:#}");
            }
        }

        let run_started = SystemTime::now();
        let container = docker::run_detached(&args)?;

        if opts.detach {
            // vault was rejected above, so there's no host-side proxy to keep
            // alive: record the session and return. Reattach later with the
            // same docker-exec relay via `pillbox session attach <id>`.
            let session = Session {
                id: Session::new_id(),
                label: opts.label.clone(),
                remote: "local".to_string(),
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
        // Open the durable per-session log (the spine's first producer). The
        // agent's $HOME is host-bind-mounted, so the transcript — and thus the
        // log — is written host-side and persists in ~/.pillbox. Best-effort:
        // a log-open failure must not abort the run, so fall back to OTLP-only.
        let log = match crate::events::log::SessionLog::open(resolved, &session_id) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("pillbox: warning: couldn't open session log: {e:#}");
                None
            }
        };
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

/// Attach the terminal pump to a running pty-host container by execing the
/// per-attach relay and pumping over its stdio. `detach_enabled` is false
/// for a foreground `run` (no session to leave behind, so Ctrl-A passes
/// through and SIGTERM terminates) and true for `session attach`.
fn attach_via_exec(container: &str, detach_enabled: bool) -> Result<Outcome> {
    let mut child = docker::exec_attach(
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
    // Don't leave the relay exec lingering.
    let _ = child.kill();
    let _ = child.wait();
    Ok(outcome)
}

/// Push one `Input` frame to a running session's in-container pty-host via a
/// one-shot docker-exec relay — the same transport `attach` uses, but driven
/// programmatically (`driver::send_input`, no pump). The `SendInput` half of
/// the drive surface: bytes in, as if typed. `pillbox session send <id>`.
///
/// No `DataAck` in the frame protocol yet, so after writing we EOF the relay
/// (it forwards the buffered frame, then sees end-of-input) and wait a bounded
/// beat for the pty-host to apply it to the PTY before tearing the exec down —
/// a timed best-effort, not a delivery confirmation. A future ack frame turns
/// this into a real round-trip.
pub(crate) fn send_input(container: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let mut child = docker::exec_attach(
        container,
        &[
            "pillbox".into(),
            "pty-relay".into(),
            "--sock".into(),
            ATTACH_SOCK.into(),
        ],
    )?;
    let mut stdin = child.stdin.take().context("docker exec relay stdin")?;
    crate::attach::driver::send_input(&mut stdin, bytes)
        .context("write Input frame to the relay")?;
    stdin.flush().ok();
    drop(stdin); // EOF the relay once it has read the buffered frame
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
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
        Outcome::Detached => {
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
        Outcome::Disconnected => {
            eprintln!("pillbox: session connection closed.");
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
