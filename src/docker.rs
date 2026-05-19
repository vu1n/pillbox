//! Thin wrapper over the `docker` CLI.
//!
//! Shells out instead of using a Rust SDK to keep the runtime dep at
//! "Docker Desktop installed" — no extra crates, no extra failure modes.

use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result};

use crate::errors::PillboxError;

/// Published runner image tag. For v0.1 this is the locally-built lum image
/// (named `pillbox` in this session). v0.2 publishes to GHCR.
pub const RUNNER_IMAGE: &str = "pillbox:latest";

/// Confirm Docker is reachable and the runner image is available. One
/// subprocess call instead of two: `docker image inspect` fails with a
/// clear "Cannot connect to Docker daemon" message if the daemon is
/// down, and with a clear "No such image" message if the image is
/// missing — we just translate each into a more actionable hint.
pub fn check_ready() -> Result<()> {
    let out = Command::new("docker")
        .arg("image")
        .arg("inspect")
        .arg(RUNNER_IMAGE)
        .arg("--format")
        .arg("{{.Id}}")
        .output()
        .context("running `docker` — is Docker Desktop installed?")?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Cannot connect to the Docker daemon") {
        return Err(
            PillboxError::resource("docker pre-flight", "Docker daemon isn't running")
                .with_next("start Docker Desktop, then re-run pillbox")
                .into(),
        );
    }
    if stderr.contains("No such image") || stderr.contains("Error response from daemon") {
        return Err(PillboxError::resource(
            "docker pre-flight",
            format!("runner image `{RUNNER_IMAGE}` not found locally"),
        )
        .with_next(
            "cd ~/code/lum && bun run build:runtime-image:pillbox  # GHCR publish lands in v0.4",
        )
        .into());
    }
    Err(PillboxError::resource("docker pre-flight", stderr.into_owned()).into())
}

/// Run `docker run <args...>` with stdio inherited from the parent.
pub fn run_interactive(args: &[String]) -> Result<ExitStatus> {
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
