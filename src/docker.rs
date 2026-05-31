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

/// Built-in default runner image tag. Bumped per CLI release so a
/// fresh pillbox install picks up a matching pre-published runner.
/// Override at three levels (highest precedence first):
///   1. `PILLBOX_RUNNER_IMAGE` env var
///   2. `[runner] image = "…"` in pillbox.toml
///   3. this default
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
/// [`check_ready`] so the four backends that launch docker can't
/// accidentally check one image and `docker run` another.
pub fn check_ready_for(resolved: &Pillbox) -> Result<String> {
    check_ready_for_at(resolved, &DockerEndpoint::local())
}

/// Endpoint-aware [`check_ready_for`]: resolve the runner image for
/// `resolved`, then preflight it on `endpoint`'s daemon. The `docker://`
/// backend passes a remote endpoint so a missing image is caught against
/// the daemon that will actually run it, not the local one.
pub fn check_ready_for_at(resolved: &Pillbox, endpoint: &DockerEndpoint) -> Result<String> {
    let (image, _src) = resolve_runner_image(resolved);
    check_ready_at(endpoint, &image)?;
    Ok(image)
}

/// Probe whether the runner `image`'s **in-container** pillbox speaks the
/// `--vault-stdin-direct` launch protocol the docker:// backend uses. A runner
/// image baked from a pillbox older than that flag rejects it (clap: "unexpected
/// argument"), so the container's pty-host child exits on the parse error and
/// the agent appears to "start then immediately die" — a silent, baffling
/// failure. Catching the skew here turns it into one actionable message.
///
/// Cheap and conservative: pillbox's arg parse exits in milliseconds (only the
/// container spin-up costs anything), and we fail **only** on the specific
/// unknown-flag signal — any other probe outcome (the expected "blob file not
/// found", or success) means the flag is recognized, so the protocol is fine.
pub fn check_runner_protocol_at(endpoint: &DockerEndpoint, image: &str) -> Result<()> {
    let out = endpoint
        .command()
        .args([
            "run",
            "--rm",
            image,
            "pillbox",
            "run",
            "--vault-stdin-direct",
            "--blob-file",
            "/nonexistent-pillbox-skew-probe",
        ])
        .output()
        .context("probing runner-image protocol")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_protocol_skew(out.status.code(), &stderr) {
        return Err(PillboxError::resource(
            "docker pre-flight",
            format!(
                "runner image `{image}` is too old — its pillbox doesn't support the \
                 `--vault-stdin-direct` launch protocol, so the agent would start and \
                 immediately exit"
            ),
        )
        .with_next(format!(
            "rebuild/pull a current runner image on the daemon (e.g. `docker pull {image}`)"
        ))
        .into());
    }
    Ok(())
}

/// Did the probe show the runner's pillbox rejecting `--vault-stdin-direct` as
/// an unknown flag? clap exits **2** on any arg-parse failure; a current pillbox
/// parses the flag and fails later on the missing probe blob (exit 1, a runtime
/// error). Keying on the exit code — not clap's English "unexpected argument"
/// prose, which is version/locale-dependent and would fail *open* into the very
/// silent-death this guards against — is the robust signal; we still scope to
/// our flag name so an unrelated usage error can't masquerade as skew. Pure so
/// the match is unit-tested without a daemon.
fn is_protocol_skew(exit_code: Option<i32>, stderr: &str) -> bool {
    exit_code == Some(2) && stderr.contains("vault-stdin-direct")
}

/// Which Docker daemon a command targets — the **placement axis** of the
/// remotes redesign (see docs/remotes-redesign.md), expressed as one env
/// override. `local()` uses the ambient daemon (local socket, or whatever
/// `DOCKER_HOST`/`docker context` the user already has). `remote(host)`
/// points `DOCKER_HOST` at a daemon over SSH transport — the `docker://`
/// backend passes `ssh://[user@]host[:port]`. Container lifecycle is
/// otherwise identical: "run the runner image somewhere, attach over an
/// exec channel," parameterized only by where the daemon lives.
#[derive(Debug, Clone, Default)]
pub struct DockerEndpoint(Option<String>);

impl DockerEndpoint {
    /// The ambient daemon — no `DOCKER_HOST` override. Behaves exactly as
    /// the bare `docker` CLI would for this shell.
    pub fn local() -> Self {
        Self(None)
    }

    /// A remote daemon reached by exporting `DOCKER_HOST` (e.g.
    /// `ssh://user@host:port`) for the spawned `docker` process.
    pub fn remote(docker_host: impl Into<String>) -> Self {
        Self(Some(docker_host.into()))
    }

    /// The `DOCKER_HOST` override this endpoint exports (`Some` for remote,
    /// `None` for local) — for diagnostics / preflight messages. Named
    /// distinctly from [`crate::remote::DockerUrl::docker_host`], which
    /// *renders* an `ssh://…` string.
    pub fn host_override(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// Build a `docker` command, exporting `DOCKER_HOST` for the remote
    /// case so every invocation in the lifecycle targets the same daemon.
    /// The single place the endpoint turns into a process — keeps the
    /// `*_at` helpers from each re-deriving the env.
    fn command(&self) -> Command {
        let mut c = Command::new("docker");
        if let Some(host) = &self.0 {
            c.env("DOCKER_HOST", host);
        }
        c
    }
}

/// Confirm Docker is reachable and `image` is available on the **ambient**
/// daemon. See [`check_ready_at`] for the endpoint-aware form.
pub fn check_ready(image: &str) -> Result<()> {
    check_ready_at(&DockerEndpoint::local(), image)
}

/// Confirm Docker is reachable and `image` is available on `endpoint`'s
/// daemon. One subprocess call instead of two: `docker image inspect`
/// fails with a clear "Cannot connect to Docker daemon" message if the
/// daemon is down, and with a clear "No such image" message if the image
/// is missing — we just translate each into a more actionable hint.
pub fn check_ready_at(endpoint: &DockerEndpoint, image: &str) -> Result<()> {
    let out = endpoint
        .command()
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

/// `docker cp <container>:<src>/. <host_dest>` on `endpoint`'s daemon — copy
/// the *contents* of a container directory back to a host path (the
/// result-extraction direction; the reverse of [`cp_file_at`] /
/// [`cp_stdin_at`]). Works on a stopped container, so docker:// can pull the
/// agent's final workspace out before the container is reaped. The trailing
/// `/.` copies the directory's contents into `host_dest` rather than nesting a
/// `<src>` subdir; matching files are overwritten.
// Wired by the docker:// run assembly's result-extraction step.
#[allow(dead_code)]
pub fn cp_out_at(
    endpoint: &DockerEndpoint,
    container: &str,
    src: &str,
    host_dest: &std::path::Path,
) -> Result<()> {
    let out = endpoint
        .command()
        .arg("cp")
        .arg(format!("{container}:{src}/."))
        .arg(host_dest)
        .output()
        .context("invoking `docker cp` (out)")?;
    if out.status.success() {
        return Ok(());
    }
    Err(PillboxError::resource(
        "result extraction",
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
    .into())
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

/// `docker rm -f <container>` on the ambient daemon. See [`rm_force_at`].
pub fn rm_force(container: &str) -> Result<()> {
    rm_force_at(&DockerEndpoint::local(), container)
}

/// `docker rm -f <container>` on `endpoint`'s daemon — force-remove (kills
/// if running). Best-effort teardown for the attach-transport flow; errors
/// are the caller's to ignore.
pub fn rm_force_at(endpoint: &DockerEndpoint, container: &str) -> Result<()> {
    endpoint
        .command()
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
    exec_attach_at(&DockerEndpoint::local(), container, argv)
}

/// Endpoint-aware [`exec_attach`] — attach to a pty-relay in a container on
/// `endpoint`'s daemon. For `docker://`, `DOCKER_HOST=ssh://…` makes the
/// exec stream ride the SSH transport back to the host pump unchanged.
pub fn exec_attach_at(
    endpoint: &DockerEndpoint,
    container: &str,
    argv: &[String],
) -> Result<std::process::Child> {
    endpoint
        .command()
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

/// `docker create <args...>` on `endpoint`'s daemon — provision a container
/// WITHOUT starting it. The docker:// path needs this so the workspace can be
/// `docker cp`'d in *before* the agent runs: the ordering is
/// **create → stage → start** (you can't cp into a not-yet-created container,
/// and `run` would start the agent before its workspace exists). `args` must
/// include the image + command. Returns the container id (stdout, trimmed).
// Wired by the docker:// container lifecycle (next slice); exercised now by the
// workspace-staging ordering test.
#[allow(dead_code)]
pub fn create_at(endpoint: &DockerEndpoint, args: &[String]) -> Result<String> {
    let out = endpoint
        .command()
        .arg("create")
        .args(args)
        .output()
        .context("invoking `docker create`")?;
    if !out.status.success() {
        return Err(PillboxError::resource(
            "sandbox create",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
        .into());
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() {
        // Guard the "failure-after-Created reaps" invariant: an empty id would
        // make staging/start operate on `":dest"` and the reap a no-op, leaking
        // any container that was actually created.
        return Err(PillboxError::resource(
            "sandbox create",
            "docker create reported success but returned no container id",
        )
        .into());
    }
    Ok(id)
}

/// `docker start <container>` on `endpoint`'s daemon — start a container
/// provisioned by [`create_at`], after its workspace has been staged in.
#[allow(dead_code)]
pub fn start_at(endpoint: &DockerEndpoint, container: &str) -> Result<()> {
    let out = endpoint
        .command()
        .arg("start")
        .arg(container)
        .output()
        .context("invoking `docker start`")?;
    if out.status.success() {
        return Ok(());
    }
    Err(PillboxError::resource(
        "sandbox start",
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
    .into())
}

/// `docker run <args...>` detached on the ambient daemon. See
/// [`run_detached_at`].
pub fn run_detached(args: &[String]) -> Result<String> {
    run_detached_at(&DockerEndpoint::local(), args)
}

/// `docker run <args...>` detached on `endpoint`'s daemon. `args` must
/// include `-d` and the image + command. Returns the container id (stdout,
/// trimmed).
pub fn run_detached_at(endpoint: &DockerEndpoint, args: &[String]) -> Result<String> {
    let out = endpoint
        .command()
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

/// `docker cp - <container>:<dest>` on `endpoint`'s daemon, reading a tar
/// archive from `stdin` and extracting it under `dest` (which must already
/// exist in the container). This is how a workspace is staged into a
/// container *without* a bind-mount — the docker:// / remote path, where the
/// daemon can't see the host's cwd. `stdin` is typically the piped stdout of
/// a `tar` child, so the transfer streams rather than buffering the tree.
pub fn cp_stdin_at(
    endpoint: &DockerEndpoint,
    container: &str,
    dest: &str,
    stdin: Stdio,
) -> Result<()> {
    let out = endpoint
        .command()
        .arg("cp")
        .arg("-")
        .arg(format!("{container}:{dest}"))
        .stdin(stdin)
        .output()
        .context("invoking `docker cp -`")?;
    if out.status.success() {
        return Ok(());
    }
    Err(PillboxError::resource(
        "workspace stage",
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
    .into())
}

/// `docker cp <src> <container>:<dest>` on `endpoint`'s daemon — copy a single
/// host file into the container (e.g. the staged vault/auth blob). Distinct
/// from [`cp_stdin_at`], which extracts a tar archive from stdin; this is the
/// plain file-in form.
// Wired by the docker:// run assembly (next slice) to stage the blob.
#[allow(dead_code)]
pub fn cp_file_at(
    endpoint: &DockerEndpoint,
    src: &std::path::Path,
    container: &str,
    dest: &str,
) -> Result<()> {
    let out = endpoint
        .command()
        .arg("cp")
        .arg(src)
        .arg(format!("{container}:{dest}"))
        .output()
        .context("invoking `docker cp <file>`")?;
    if out.status.success() {
        return Ok(());
    }
    Err(PillboxError::resource(
        "workspace stage",
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
    .into())
}

/// `docker inspect` the container's run state on `endpoint`'s daemon.
/// Returns `Some(exit_code)` if it has exited, `None` if it's still running
/// (or the inspect failed). Lets the docker:// backend report a fast-exiting
/// agent's real status when the attach relay couldn't reach a container that
/// already stopped.
#[allow(dead_code)]
pub fn container_exit_code_at(endpoint: &DockerEndpoint, container: &str) -> Option<i32> {
    let out = endpoint
        .command()
        .arg("inspect")
        .arg("-f")
        .arg("{{.State.Running}} {{.State.ExitCode}}")
        .arg(container)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut fields = stdout.split_whitespace();
    let running = fields.next()? == "true";
    let code: i32 = fields.next()?.parse().ok()?;
    (!running).then_some(code)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The skew predicate fires on the real clap "unexpected argument" stderr a
    /// stale runner image emits, and stays quiet for a current image (which
    /// fails on the probe's missing blob — flag recognized). Strings are the
    /// actual captured outputs from a stale vs current runner image.
    #[test]
    fn protocol_skew_keys_on_clap_exit_code() {
        // Stale image: clap rejects the unknown flag → exit 2, flag named.
        let stale = "error: unexpected argument '--vault-stdin-direct' found\n";
        assert!(
            is_protocol_skew(Some(2), stale),
            "stale image (clap exit 2) should be detected"
        );
        // Current image: flag parsed, fails on the missing probe blob → exit 1.
        let current =
            "pillbox: run --vault-stdin-direct failed. read blob file /nope: No such file or directory\n";
        assert!(
            !is_protocol_skew(Some(1), current),
            "current image (runtime exit 1) must not be flagged"
        );
        // An unrelated usage error (exit 2) that doesn't name our flag isn't skew.
        assert!(!is_protocol_skew(
            Some(2),
            "error: unexpected argument '--bogus'\n"
        ));
        assert!(!is_protocol_skew(None, ""), "no exit code is not skew");
    }

    /// `local()` must not inject a `DOCKER_HOST` — the command behaves
    /// exactly as the bare `docker` CLI for the user's shell.
    #[test]
    fn local_endpoint_sets_no_docker_host() {
        let cmd = DockerEndpoint::local().command();
        assert!(cmd
            .get_envs()
            .all(|(k, _)| k != std::ffi::OsStr::new("DOCKER_HOST")));
        assert_eq!(DockerEndpoint::local().host_override(), None);
    }

    /// `remote(host)` exports `DOCKER_HOST` so every lifecycle call in the
    /// `docker://` path targets the same remote daemon.
    #[test]
    fn remote_endpoint_exports_docker_host() {
        let ep = DockerEndpoint::remote("ssh://deploy@vps:2222");
        assert_eq!(ep.host_override(), Some("ssh://deploy@vps:2222"));
        let cmd = ep.command();
        let dh = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("DOCKER_HOST"))
            .and_then(|(_, v)| v)
            .expect("DOCKER_HOST set");
        assert_eq!(dh, std::ffi::OsStr::new("ssh://deploy@vps:2222"));
    }
}
