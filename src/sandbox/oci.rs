//! The runner image: which OCI image a microVM rootfs is built from, and the
//! `docker` CLI calls that resolve and export it.
// docker here is image plumbing for the microVM rootfs, never an agent-run backend; this is the only file that runs `docker`.

use std::path::Path;
use std::process::{Command, ExitStatus};

use anyhow::{bail, Context, Result};

use crate::pillbox::Pillbox;

/// Default runner image (stable tag). Override with `PILLBOX_RUNNER_IMAGE` or
/// `[runner] image` in pillbox.toml — e.g. `:rolling` for the latest dev build,
/// or a pinned `:vX.Y.Z`. Tag scheme + publishing: docs/runner-image.md.
pub const DEFAULT_RUNNER_IMAGE: &str = "ghcr.io/vu1n/pillbox-runner:latest";

/// Env-var override key. Documented so `pillbox doctor` can name it
/// in its "source" attribution.
pub const RUNNER_IMAGE_ENV: &str = "PILLBOX_RUNNER_IMAGE";

/// The `tar` extract flags a rootfs export is unpacked with: `-p` keeps the
/// image's modes (sticky, world-writable `/tmp`) exactly.
pub(crate) const ROOTFS_TAR_EXTRACT_FLAG: &str = "-xpf";

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
/// Precedence: env var > `pillbox.toml [runner] image` (project, then
/// `~/.pillbox/global/pillbox.toml`) > built-in default. The toml reads
/// happen on demand (per `pillbox run`) so editing a descriptor takes
/// effect immediately — no meta.json rewrite dance — and the parse cost
/// is negligible against the sandbox spawn that follows.
pub fn resolve_runner_image(resolved: &Pillbox) -> (String, RunnerImageSource) {
    if let Ok(env) = std::env::var(RUNNER_IMAGE_ENV) {
        return (env, RunnerImageSource::Env);
    }
    // Descriptor cascade: project `pillbox.toml` (walk-up) overlaid on the
    // user-global `~/.pillbox/global/pillbox.toml`.
    let cfg = crate::config::resolve_run_config(resolved);
    if let Some(image) = cfg.runner.image {
        if !image.trim().is_empty() {
            return (image, RunnerImageSource::ProjectToml);
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

/// Best-effort enumeration of runner images available locally — for the
/// `pillbox new -i` image selection. Two sources, deduped + sorted: local docker
/// `pillbox-runner:*` tags, and the cached libkrun rootfs under
/// `~/.pillbox/krun/rootfs/` (dir `pillbox_runner_dev` ↔ tag `pillbox-runner:dev`;
/// the `…_sha256_…` by-digest variants are skipped — they aren't a usable tag).
/// Display-only: empty on any failure, and the wizard always offers a free-text
/// fallback, so this is never a correctness path.
pub fn list_runner_images() -> Vec<String> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<String> = BTreeSet::new();

    if let Ok(out) = Command::new("docker")
        .args(["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.starts_with("pillbox-runner:") && !line.ends_with(":<none>") {
                set.insert(line.to_string());
            }
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        let rootfs = std::path::Path::new(&home).join(".pillbox/krun/rootfs");
        if let Ok(rd) = std::fs::read_dir(&rootfs) {
            for entry in rd.flatten() {
                if let Some(tag) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix("pillbox_runner_"))
                {
                    if !tag.is_empty() && !tag.contains("_sha256_") {
                        set.insert(format!("pillbox-runner:{tag}"));
                    }
                }
            }
        }
    }

    set.into_iter().collect()
}

/// The local image id (`sha256:…`) `image` resolves to.
pub(crate) fn image_id(image: &str) -> Result<String> {
    let out = inspect_id_command(image)
        .output()
        .with_context(|| format!("inspect runner image {image}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    bail!(
        "docker image inspect {image} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// `docker image inspect <image> --format {{.Id}}`, unrun — for a caller that
/// runs it under its own deadline.
pub(crate) fn inspect_id_command(image: &str) -> Command {
    let mut command = Command::new("docker");
    command.args(["image", "inspect", image, "--format", "{{.Id}}"]);
    command
}

/// Whether the Docker daemon (or the `docker` CLI itself) is unreachable, probed
/// through an inspect of `image` — as opposed to the image merely being absent.
pub(crate) fn daemon_unreachable(image: &str) -> bool {
    let stderr = inspect_id_command(image)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default();
    // Empty stderr ⇒ the `docker` invocation itself couldn't run (binary missing /
    // exec error) ⇒ treat as unreachable, same as an explicit connect error.
    stderr.is_empty() || stderr.contains("Cannot connect to the Docker daemon")
}

/// `docker create <image>`: a stopped container whose filesystem can be exported.
/// Returns its id; the caller removes it with [`remove_container`].
pub(crate) fn create_container(image: &str) -> Result<String> {
    let create = Command::new("docker")
        .args(["create", image])
        .output()
        .context("docker create (is the runner image present + docker running?)")?;
    if !create.status.success() {
        bail!(
            "docker create {image} failed: {}",
            String::from_utf8_lossy(&create.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&create.stdout).trim().to_string())
}

/// Best-effort `docker rm -f <container>`, output captured (it echoes the id, and
/// `run --json`'s stdout must stay pure JSON).
pub(crate) fn remove_container(container: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", container])
        .output();
}

/// Stream `container`'s filesystem into the existing directory `dest`
/// (`docker export | tar -x`), both commands' stdio captured. The status is the
/// pipeline's (tar's).
pub(crate) fn export_container_into(container: &str, dest: &Path) -> std::io::Result<ExitStatus> {
    Command::new("sh")
        .arg("-c")
        .arg(format!(
            "docker export {} | tar -C {} {ROOTFS_TAR_EXTRACT_FLAG} -",
            shell_quote(container),
            shell_quote(&dest.to_string_lossy())
        ))
        .output()
        .map(|o| o.status)
}

/// `docker create --name <name> <image>`, unrun.
pub(crate) fn create_named_command(name: &str, image: &str) -> Command {
    let mut command = Command::new("docker");
    command.args(["create", "--name", name, image]);
    command
}

/// `docker export --output <archive> <container>`, unrun.
pub(crate) fn export_to_file_command(container: &str, archive: &Path) -> Command {
    let mut command = Command::new("docker");
    command
        .args(["export", "--output"])
        .arg(archive)
        .arg(container);
    command
}

/// `docker rm --force <container>`, unrun.
pub(crate) fn remove_command(container: &str) -> Command {
    let mut command = Command::new("docker");
    command.args(["rm", "--force", container]);
    command
}

/// The runner image's local id for `pillbox doctor`'s export-readiness check:
/// `Ok(Some(id))` present, `Ok(None)` absent, `Err` when `docker` can't run.
pub(crate) fn probe_image(image: &str) -> Result<Option<String>> {
    let out = inspect_id_command(image)
        .output()
        .context("docker CLI unavailable")?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Single-quote a shell argument (`'` → `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
