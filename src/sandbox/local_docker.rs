//! `SandboxBackend` implementation that uses the host's Docker daemon.
//!
//! v0.6: takes a resolved [`Pillbox`] so the agent's auth home + vault
//! state come from the right scope. Auth currently always resolves to
//! global; vault state lives per-pillbox so a project's leases never
//! collide with another's.

use anyhow::{Context, Result};

use super::SandboxBackend;
use crate::agents::{
    base_docker_args_detached, resolve_run_env, resolve_with_entries, workspace_mount_name,
    AgentSpec, RunOpts, GUEST_HOME, GUEST_WORKSPACE,
};
use crate::attach::pump::{self, Outcome};
use crate::pillbox::Pillbox;
use crate::workspace::WorkspaceBackend;
use crate::{docker, errors::PillboxError};

pub(crate) struct LocalDocker;

/// Where the in-container pty-host listens; the per-attach relay (run via
/// `docker exec`) connects to the same path. One pty-host per container.
const ATTACH_SOCK: &str = "/tmp/pillbox-attach.sock";

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
        let mut vault_session = if opts.vault || any_vaulted {
            let oauth = if opts.vault {
                Some(crate::vault::OAuthAgent {
                    agent_id: spec.id,
                    agent_home: &home,
                })
            } else {
                None
            };
            Some(crate::vault::VaultSession::start(oauth, resolved)?)
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

        // Local --detach (a persisted session) isn't wired yet — it stays
        // remote-only, matching today's contract. Sessions land next.
        if opts.detach {
            return Err(
                PillboxError::usage("run", "--detach is not supported for local runs yet")
                    .with_next("pillbox run --detach --remote <name>")
                    .into(),
            );
        }

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
        args.extend(opts.args);

        let container = docker::run_detached(&args)?;
        let outcome = attach_via_exec(&container);
        // The vault proxy must stay up for the whole attached session.
        drop(vault_session);
        // Foreground run: tear the container down regardless of outcome.
        let _ = docker::rm_force(&container);

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
/// per-attach relay and pumping over its stdio.
fn attach_via_exec(container: &str) -> Result<Outcome> {
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
    let outcome = pump::attach_terminal(stdout, stdin)?;
    // Don't leave the relay exec lingering.
    let _ = child.kill();
    let _ = child.wait();
    Ok(outcome)
}
