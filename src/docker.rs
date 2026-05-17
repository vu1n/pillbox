//! Thin wrapper over the `docker` CLI.
//!
//! v0.1 shells out to docker rather than using a Rust SDK like bollard so
//! the runtime dep stays at "Docker Desktop installed" — no extra crates,
//! no extra failure modes. Trade-off: less typed error handling.

use std::process::{Command, ExitStatus, Stdio};

use anyhow::{anyhow, Context, Result};

/// The published runner image. For v0.1 we point at the same Dockerfile
/// lum uses (renamed to `pillbox` in this session). Once we publish to
/// GHCR this becomes a stable tag.
pub const RUNNER_IMAGE: &str = "pillbox:latest";

/// Verify Docker is installed and reachable.
pub fn check_available() -> Result<()> {
    let out = Command::new("docker")
        .arg("version")
        .arg("--format")
        .arg("{{.Server.Version}}")
        .output()
        .context(
            "running `docker version` — is Docker Desktop installed and running?",
        )?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "docker is installed but the daemon isn't responding:\n{stderr}"
        ));
    }
    Ok(())
}

/// Confirm the runner image is available locally. We do NOT auto-pull in
/// v0.1 — pull failure is a separate UX problem and we want users to see
/// the explicit "image not found" message until publishing is set up.
pub fn check_image() -> Result<()> {
    let out = Command::new("docker")
        .arg("image")
        .arg("inspect")
        .arg(RUNNER_IMAGE)
        .output()
        .context("checking runner image")?;
    if !out.status.success() {
        return Err(anyhow!(
            "runner image `{RUNNER_IMAGE}` not found locally.\n\n\
             For v0.1, build it from the lum repo:\n  \
             cd ~/code/lum && bun run build:runtime-image:pillbox\n\n\
             A published GHCR image is on the v0.2 roadmap."
        ));
    }
    Ok(())
}

/// Run an interactive Docker container, attaching the current process's
/// stdin/stdout/stderr. Blocks until the container exits.
///
/// `args` is the full argv after `docker run` (e.g. `["-it", "--rm",
/// "-v", "...", "pillbox:latest", "claude", "/login"]`).
pub fn run_interactive(args: &[&str]) -> Result<ExitStatus> {
    let status = Command::new("docker")
        .arg("run")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("invoking `docker run`")?;
    Ok(status)
}
