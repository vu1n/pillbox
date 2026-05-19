//! `SandboxBackend` implementation that uses the host's Docker daemon.
//!
//! v0.6: takes a resolved [`Pillbox`] so the agent's auth home + vault
//! state come from the right scope. Auth currently always resolves to
//! global; vault state lives per-pillbox so a project's leases never
//! collide with another's.

use anyhow::{Context, Result};

use super::SandboxBackend;
use crate::agents::{
    base_docker_args, resolve_run_env, resolve_with_entries, workspace_mount_name, AgentSpec,
    RunOpts, GUEST_HOME, GUEST_WORKSPACE,
};
use crate::pillbox::Pillbox;
use crate::{docker, errors::PillboxError};

pub(crate) struct LocalDocker;

impl SandboxBackend for LocalDocker {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        docker::check_ready()?;

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

        let mut args = base_docker_args();
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
        for (k, v) in &env_vars {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        if let Some(session) = &vault_session {
            args.extend(session.docker_extras(GUEST_HOME));
            eprintln!(
                "pillbox: vault proxy listening on {} (ca: {})",
                session.listen_addr(),
                session.ca_cert_path().display()
            );
        }
        args.push(docker::RUNNER_IMAGE.into());
        args.extend(spec.run_argv.iter().map(|s| s.to_string()));
        args.extend(opts.args);

        let status = docker::run_interactive(&args)?;
        drop(vault_session);
        if !status.success() {
            return Err(PillboxError::runtime(
                "run",
                format!("{} exited with status {status}", spec.id),
            )
            .into());
        }
        Ok(())
    }
}
