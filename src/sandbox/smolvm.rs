//! SPIKE: `SandboxBackend` over the `smolvm` CLI (smol-machines/smolvm,
//! Apache-2.0) — evaluating whether we adopt smolvm as the local microVM
//! substrate and retire our hand-rolled libkrun L1–L7, spending the freed
//! budget on the §0/session/multiplayer layer smolvm doesn't have.
//!
//! Scope of this spike: the **foreground, non-vault, interactive** path only —
//! boot the runner image in an ephemeral smolvm machine with the workspace +
//! agent home bind-mounted, run the agent under smolvm's own `-it` PTY, inherit
//! exit. Deliberately NOT here (the parts that decide adopt-vs-maintain — see
//! the module-level analysis in docs/managed-tier.md):
//!   - **vault** — smolvm has no host-side MITM-of-guest-egress hook (only
//!     allowlist + secret-injection-into-guest), so the "real credential never
//!     enters the guest" boundary (our L5/L6) can't ride smolvm as-is. This is
//!     the one capability that argues for keeping our own libkrun.
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
        if opts.vault || opts.detach || opts.memory {
            return Err(unsupported(
                "vault / detach / memory — the §0 + MITM-vault work, not in the substrate spike",
            ));
        }
        let withs_resolved = resolve_with_entries(resolved, &opts.withs)?;
        if withs_resolved.iter().any(|w| w.meta.is_some()) {
            return Err(unsupported("vaulted `--with` secrets — same as --vault"));
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

        let env_vars = resolve_run_env(resolved, &opts, &withs_resolved, None)?;

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
