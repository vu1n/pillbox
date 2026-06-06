//! SPIKE: `SandboxBackend` over the `smolvm` CLI (smol-machines/smolvm,
//! Apache-2.0) — evaluating whether we adopt smolvm as the local microVM
//! substrate and retire our hand-rolled libkrun L1–L7, spending the freed
//! budget on the §0/session/multiplayer layer smolvm doesn't have.
//!
//! Scope of this spike: the **foreground interactive** path — boot the runner
//! image in an ephemeral smolvm machine with the workspace + agent home
//! bind-mounted, run the agent under smolvm's own `-it` PTY, inherit exit.
//!
//! **Vault works** via the **explicit-proxy broker** — the same model as docker
//! (`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` → a host-side MITM proxy; the real key
//! never enters the guest, only a stub). This needs **no transparent network
//! interception**, so no smolvm change is required for proxy-honoring agents
//! (claude/codex/node — what we target). The one unverified bit is
//! `PILLBOX_SMOLVM_HOST_ADDR`: how the guest addresses the host proxy (docker
//! uses `host.docker.internal`; defaults to 127.0.0.1). A *transparent*
//! egress-redirect (for traffic that ignores `HTTP_PROXY`) is the only case that
//! would want the upstream `tcp_relay` hook or our own libkrun — not v1.
//!
//! Deliberately NOT here:
//!   - **§0 / pty-host / attach / detach** — the daylight; layered over any
//!     backend via a guest-side pty-host reachable on a smolvm port-mapping.
//!
//! Opt in with `PILLBOX_BACKEND=smolvm` (feature `smolvm`).

use anyhow::{Context, Result};

use super::SandboxBackend;
use crate::agents::{
    resolve_run_env, resolve_with_entries, workspace_mount_name, AgentSpec, Integration, RunOpts,
    GUEST_HOME, GUEST_WORKSPACE,
};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::workspace::WorkspaceBackend;
use crate::{docker, smolvm};

pub(crate) struct SmolvmBackend;

impl SandboxBackend for SmolvmBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Spike boundaries: surface the unimplemented paths loudly rather than
        // silently mis-running them. Each maps to a real adopt-vs-maintain item.
        if spec.integration == Integration::Server || spec.server.is_some() {
            return Err(unsupported(
                "server-mode agents (opencode/codex-serve) — §0 path",
            ));
        }
        if opts.detach || opts.memory {
            return Err(unsupported(
                "detach / memory — the §0 path, not in the substrate spike",
            ));
        }
        let withs_resolved = resolve_with_entries(resolved, &opts.withs)?;
        let any_vaulted = withs_resolved.iter().any(|w| w.meta.is_some());
        if (opts.vault || any_vaulted) && !spec.vault_capable {
            return Err(PillboxError::usage(
                "run",
                format!("--vault is not supported for `{}`", spec.id),
            )
            .into());
        }

        // Image: same OCI ref the docker/libkrun backends use; smolvm pulls it
        // from the registry itself (no daemon).
        let (runner_image, _src) = docker::resolve_runner_image(resolved);

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

        // Vault via the explicit-proxy broker (same model as docker): start a
        // host-side MITM proxy, stub the credentials, and point the guest at it
        // with HTTPS_PROXY + the trusted CA (see `smolvm_extras` below). The real
        // key never enters the guest — no transparent interception, so no smolvm
        // change is needed for proxy-honoring agents. Detach is rejected above, so
        // the host proxy always outlives the run (same constraint docker has).
        let mut vault_session = if opts.vault || any_vaulted {
            let oauth = opts.vault.then_some(crate::vault::OAuthAgent {
                agent_id: spec.id,
                agent_home: &home,
            });
            let context = crate::vault::RunContext {
                session_id: None,
                mode: Some("interactive".to_string()),
                workspace_id: Some(resolved.workspace_id().to_string()),
            };
            Some(crate::vault::VaultSession::start(oauth, resolved, context)?)
        } else {
            None
        };
        let env_vars = resolve_run_env(resolved, &opts, &withs_resolved, vault_session.as_mut())?;

        // `smolvm machine run -it -I <image> -v ... -e ... -- <agent argv>`.
        // `--net` opens egress (no fence in the spike — egress-fence + vault are
        // the libkrun/managed path's job). `-v host:guest` matches docker's form.
        let mut args: Vec<String> = vec![
            "machine".into(),
            "run".into(),
            "-it".into(),
            "--net".into(),
            "-I".into(),
            runner_image,
            "-v".into(),
            format!("{}:{GUEST_HOME}", home.display()),
            "-v".into(),
            format!("{}:{guest_workspace}", workspace_host.display()),
        ];
        for m in &opts.mounts {
            args.push("-v".into());
            args.push(m.clone());
        }
        for (k, v) in &env_vars {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        if let Some(vs) = &vault_session {
            // `--net` (open egress) above means reaching the host proxy needs no
            // extra allowlist; the addr itself is the live-verify unknown (see
            // `smolvm_extras`).
            let proxy_host = std::env::var("PILLBOX_SMOLVM_HOST_ADDR")
                .unwrap_or_else(|_| "127.0.0.1".to_string());
            args.extend(vs.smolvm_extras(GUEST_HOME, &proxy_host));
            eprintln!(
                "pillbox: [smolvm spike] vault proxy on {} (guest reaches host at {proxy_host}; ca {})",
                vs.listen_addr(),
                vs.ca_cert_path().display()
            );
        }
        args.push("--".into());
        args.extend(spec.run_argv.iter().map(|s| s.to_string()));
        args.extend(spec.sandbox_args.iter().map(|s| s.to_string()));
        args.extend(opts.args);

        // Pre-accept the agent's workspace-trust dialog on the bind-mounted home
        // (claude), same as the docker/libkrun backends.
        spec.prepare_workspace_or_warn(&home, &guest_workspace);

        eprintln!(
            "pillbox: [smolvm spike] launching `{}` in an ephemeral microVM",
            spec.id
        );
        let status = smolvm::run_interactive(&args)?;
        if status.success() {
            Ok(())
        } else {
            Err(PillboxError::runtime("run", format!("agent exited with status {status}")).into())
        }
    }
}

fn unsupported(what: &str) -> anyhow::Error {
    PillboxError::usage(
        "run",
        format!("smolvm backend spike does not support {what} yet"),
    )
    .with_next("use the default Docker backend, or PILLBOX_BACKEND=libkrun")
    .into()
}
