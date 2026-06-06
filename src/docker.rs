//! Thin wrapper over the `docker` CLI.
//!
//! Shells out instead of using a Rust SDK to keep the runtime dep at
//! "Docker Desktop installed" — no extra crates, no extra failure modes.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result};

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;

/// Default runner image (stable tag). Override with `PILLBOX_RUNNER_IMAGE` or
/// `[runner] image` in pillbox.toml — e.g. `:rolling` for the latest dev build,
/// or a pinned `:vX.Y.Z`. Tag scheme + publishing: docs/runner-image.md.
pub const DEFAULT_RUNNER_IMAGE: &str = "ghcr.io/vu1n/pillbox-runner:latest";

/// Env-var override key. Documented so `pillbox doctor` can name it
/// in its "source" attribution.
pub const RUNNER_IMAGE_ENV: &str = "PILLBOX_RUNNER_IMAGE";

/// Where a resolved runner-image string came from. Surfaced by
/// `pillbox doctor` so users can tell at a glance whether their
/// override is being picked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerImageSource {
    /// `PILLBOX_RUNNER_IMAGE` env var.
    Env,
    /// `[runner] image` in the pillbox's `pillbox.toml`.
    ProjectToml,
    /// Built-in [`DEFAULT_RUNNER_IMAGE`].
    Default,
}

impl RunnerImageSource {
    /// Short label for human-facing output (e.g. doctor's
    /// `[from pillbox.toml]` suffix).
    pub fn human(&self) -> &'static str {
        match self {
            Self::Env => "$PILLBOX_RUNNER_IMAGE",
            Self::ProjectToml => "pillbox.toml",
            Self::Default => "default",
        }
    }
}

/// Resolve the runner image without a [`Pillbox`] in hand — env var
/// or [`DEFAULT_RUNNER_IMAGE`]. Used by surfaces that don't carry
/// pillbox-resolution context (e.g. `pillbox version` banner).
pub fn default_runner_image() -> String {
    resolve_env_or_default().0
}

/// Resolve the runner image for a specific pillbox plus its source.
/// Precedence: env var > `pillbox.toml [runner] image` > built-in
/// default. The toml read happens on demand (per `pillbox run`) so
/// editing `pillbox.toml` takes effect immediately — no meta.json
/// rewrite dance — and the parse cost is negligible against the
/// docker spawn that follows.
///
/// The toml step is a no-op for the global pillbox (no descriptor
/// file to read).
pub fn resolve_runner_image(resolved: &Pillbox) -> (String, RunnerImageSource) {
    if let Ok(env) = std::env::var(RUNNER_IMAGE_ENV) {
        return (env, RunnerImageSource::Env);
    }
    if let crate::pillbox::Scope::Project { source_dir, .. } = &resolved.scope {
        let toml_path = source_dir.join("pillbox.toml");
        if let Ok(cfg) = crate::config::Config::load_from(&toml_path) {
            if let Some(image) = cfg.runner.image {
                if !image.trim().is_empty() {
                    return (image, RunnerImageSource::ProjectToml);
                }
            }
        }
    }
    (DEFAULT_RUNNER_IMAGE.to_string(), RunnerImageSource::Default)
}

/// Shared env-or-default lookup so [`default_runner_image`] and
/// [`resolve_runner_image`]'s env arm don't drift.
fn resolve_env_or_default() -> (String, RunnerImageSource) {
    match std::env::var(RUNNER_IMAGE_ENV) {
        Ok(v) => (v, RunnerImageSource::Env),
        Err(_) => (DEFAULT_RUNNER_IMAGE.to_string(), RunnerImageSource::Default),
    }
}

/// Resolve + pre-flight the runner image for `resolved`, returning
/// the resolved image string. Combines [`resolve_runner_image`] and
/// [`check_ready`] so the backends that launch docker can't
/// accidentally check one image and `docker run` another.
pub fn check_ready_for(resolved: &Pillbox) -> Result<String> {
    let (image, _src) = resolve_runner_image(resolved);
    check_ready(&image)?;
    Ok(image)
}

/// Confirm Docker is reachable and `image` is available on the local
/// daemon. One subprocess call instead of two: `docker image inspect`
/// fails with a clear "Cannot connect to Docker daemon" message if the
/// daemon is down, and with a clear "No such image" message if the image
/// is missing — we just translate each into a more actionable hint.
pub fn check_ready(image: &str) -> Result<()> {
    let out = Command::new("docker")
        .arg("image")
        .arg("inspect")
        .arg(image)
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
            format!("runner image `{image}` not found locally"),
        )
        .with_next(format!("docker pull {image}"))
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

/// `docker rm -f <container>` — force-remove (kills if running). Best-effort
/// teardown for the attach-transport flow; errors are the caller's to ignore.
/// Tolerates a missing container (see [`rm`] for the error-surfacing form).
pub fn rm_force(container: &str) -> Result<()> {
    Command::new("docker")
        .arg("rm")
        .arg("-f")
        .arg(container)
        .output()
        .context("invoking `docker rm -f`")?;
    Ok(())
}

/// `docker exec -i <container> <argv...>` with stdin + stdout piped (no TTY,
/// so the byte stream is binary-clean for the attach-transport frames) and
/// stderr inherited for diagnostics. Returns the live [`Child`]; the caller
/// takes its stdin/stdout to drive the pump. Used to attach to an
/// in-container `pillbox pty-relay`.
pub fn exec_attach(container: &str, argv: &[String]) -> Result<std::process::Child> {
    Command::new("docker")
        .arg("exec")
        .arg("-i")
        .arg(container)
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("invoking `docker exec -i`")
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

/// `docker exec -d <container> <argv...>` — start a process in the container
/// detached (returns as soon as it's launched). Used to bring up a long-lived
/// in-sandbox HTTP server (`opencode serve`) that the serve driver then talks
/// to over the loopback inside the container.
pub fn exec_detached(container: &str, argv: &[String]) -> Result<()> {
    let out = Command::new("docker")
        .arg("exec")
        .arg("-d")
        .arg(container)
        .args(argv)
        .output()
        .context("invoking `docker exec -d`")?;
    if out.status.success() {
        return Ok(());
    }
    Err(PillboxError::runtime(
        "sandbox serve",
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
    .into())
}

/// `docker exec <container> <argv...>`, capturing stdout to a `String`.
/// Returns `(exit_code, stdout)`. Used for one-shot REST calls (curl) against
/// the in-sandbox server — not for streaming (use [`exec_streamed`]).
pub fn exec_capture(container: &str, argv: &[String]) -> Result<(i32, String)> {
    let out = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(argv)
        .stdin(Stdio::null())
        .output()
        .context("invoking `docker exec`")?;
    let code = out.status.code().unwrap_or(-1);
    Ok((code, String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// `docker exec <container> <argv...>`, streaming stdout line by line until
/// `on_line` returns `Ok(true)` ("stop"), the stream ends, or an error.
/// Kills the child on an explicit stop so a long-lived stream (an SSE
/// subscription) doesn't outlive the turn that consumed it. Returns once the
/// child is reaped. stderr is discarded — the SSE consumers we drive emit
/// their protocol on stdout and only diagnostics on stderr.
pub fn exec_stream_lines(
    container: &str,
    argv: &[String],
    mut on_line: impl FnMut(&str) -> Result<bool>,
) -> Result<()> {
    use std::io::BufRead;

    let mut child = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("invoking `docker exec` (stream)")?;

    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::io::BufReader::new(stdout);
    let mut result = Ok(());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        match on_line(&line) {
            Ok(false) => {}
            Ok(true) => break, // explicit stop — kill the child below
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    // The stream may still be open (SSE never EOFs on its own); kill + reap.
    let _ = child.kill();
    let _ = child.wait();
    result
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
