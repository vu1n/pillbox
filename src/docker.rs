//! Thin wrapper over the `docker` CLI.
//!
//! Shells out instead of using a Rust SDK to keep the runtime dep at
//! "Docker Desktop installed" — no extra crates, no extra failure modes.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;

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

/// `docker run <args...>` detached. `args` must include `-d` and the image +
/// command. Returns the container id (stdout, trimmed).
pub fn run_detached(args: &[String]) -> Result<String> {
    let out = Command::new("docker")
        .arg("run")
        .args(args)
        .output()
        .context("invoking `docker run -d`")?;
    if !out.status.success() {
        return Err(PillboxError::resource(
            "sandbox spawn",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `docker exec <container> <argv...>`, streaming output in real time.
/// `on_chunk(is_stderr, bytes)` fires per read; returns the command's exit
/// code. stdout and stderr are pumped on separate threads into one ordered
/// channel so output interleaves as it arrives (no PTY).
pub fn exec_streamed(
    container: &str,
    argv: &[String],
    mut on_chunk: impl FnMut(bool, &[u8]) -> Result<()>,
) -> Result<i32> {
    let mut child = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("invoking `docker exec`")?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    let tx_err = tx.clone();
    let err_thread = std::thread::spawn(move || pump(stderr, true, &tx_err));
    // Move the original `tx` into the stdout pump so the channel closes
    // once BOTH pumps finish (no lingering sender in this scope).
    let out_thread = std::thread::spawn(move || pump(stdout, false, &tx));

    let mut result = Ok(());
    for (is_stderr, buf) in rx {
        if let Err(e) = on_chunk(is_stderr, &buf) {
            result = Err(e);
            break;
        }
    }
    out_thread.join().ok();
    err_thread.join().ok();
    result?;

    let status = child.wait().context("waiting on `docker exec`")?;
    Ok(status.code().unwrap_or(-1))
}

/// `docker rm -f <container>`. Idempotent — a missing container is `Ok`.
pub fn rm(container: &str) -> Result<()> {
    let out = Command::new("docker")
        .arg("rm")
        .arg("-f")
        .arg(container)
        .output()
        .context("invoking `docker rm -f`")?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("No such container") {
        return Ok(());
    }
    Err(PillboxError::runtime("sandbox destroy", stderr.trim().to_string()).into())
}

fn pump<R: Read>(mut reader: R, is_stderr: bool, tx: &mpsc::Sender<(bool, Vec<u8>)>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send((is_stderr, buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
}
