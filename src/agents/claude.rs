//! Claude Code adapter.
//!
//! Login: bind-mount a host tempdir at `/home/lum/.claude`, run
//! `claude auth login --claudeai` inside the runner image, then read
//! `<tempdir>/.credentials.json` and persist its contents to the OS
//! keychain under provider id `claude`.
//!
//! Run: stage stored credentials into a host tempdir, bind-mount that
//! at `/home/lum/.claude`, bind-mount the current working directory at
//! `/workspace`, and exec `claude <args>` inside the runner image.

use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};

use crate::{docker, keychain};

const PROVIDER: &str = "claude";
const GUEST_HOME: &str = "/home/lum";
const GUEST_CLAUDE_DIR: &str = "/home/lum/.claude";
const GUEST_WORKSPACE: &str = "/workspace";

/// Port Claude Code's OAuth callback listener binds. Hardcoded for v0.1;
/// auto-detection from claude's stdout is a v0.2 task if claude picks a
/// random port.
const CLAUDE_OAUTH_PORT: u16 = 54545;

pub fn login() -> Result<()> {
    docker::check_ready()?;

    let tmp = TempDir::create("pillbox-claude-login")?;
    let port_map = format!("{CLAUDE_OAUTH_PORT}:{CLAUDE_OAUTH_PORT}");
    let mount = format!("{}:{GUEST_CLAUDE_DIR}", tmp.path().display());

    println!("pillbox: starting Claude Code OAuth flow inside a sandbox.");
    println!("pillbox: a URL will print below — open it in your browser to authenticate.");
    println!();

    let mut args = base_docker_args();
    args.extend([
        "-p".into(),
        port_map,
        "-v".into(),
        mount,
        docker::RUNNER_IMAGE.into(),
        "claude".into(),
        "auth".into(),
        "login".into(),
        "--claudeai".into(),
    ]);

    let status = docker::run_interactive(&args)?;
    if !status.success() {
        return Err(anyhow!(
            "claude auth login exited with status {status}. \
             Re-run `pillbox claude login` and complete the OAuth flow."
        ));
    }

    let creds_path = tmp.path().join(".credentials.json");
    let payload = match fs::read_to_string(&creds_path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!(
                "claude auth login completed but no credentials file at {}.\n\
                 Check the sandbox output above for hints.",
                creds_path.display()
            ));
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", creds_path.display())),
    };

    keychain::save(PROVIDER, &payload)?;
    drop(tmp);

    println!();
    println!("pillbox: ✓ credentials stored in your OS keychain (service `pillbox`, account `claude`).");
    println!("pillbox: try `pillbox claude run` to launch claude in a sandboxed shell.");
    Ok(())
}

pub fn run(args: Vec<String>) -> Result<()> {
    docker::check_ready()?;

    let payload = keychain::load(PROVIDER)?
        .ok_or_else(|| anyhow!("no stored credentials for `claude`. Run `pillbox claude login` first."))?;

    let tmp = TempDir::create("pillbox-claude-creds")?;
    let creds_path = tmp.path().join(".credentials.json");
    write_secret(&creds_path, &payload)?;

    let cwd = std::env::current_dir().context("resolve current working directory")?;
    let creds_mount = format!("{}:{GUEST_CLAUDE_DIR}", tmp.path().display());
    let cwd_mount = format!("{}:{GUEST_WORKSPACE}", cwd.display());

    let mut docker_args = base_docker_args();
    docker_args.extend([
        "-v".into(),
        creds_mount,
        "-v".into(),
        cwd_mount,
        "-w".into(),
        GUEST_WORKSPACE.into(),
        docker::RUNNER_IMAGE.into(),
        "claude".into(),
    ]);
    docker_args.extend(args);

    let status = docker::run_interactive(&docker_args)?;
    drop(tmp);
    if !status.success() {
        return Err(anyhow!("claude exited with status {status}"));
    }
    Ok(())
}

/// Shared flags + env every pillbox docker invocation needs.
fn base_docker_args() -> Vec<String> {
    vec![
        "-it".into(),
        "--rm".into(),
        "-e".into(),
        format!("HOME={GUEST_HOME}"),
        "-e".into(),
        "TERM=xterm-256color".into(),
    ]
}

/// Create a file with 0600 perms from the start (no world-readable window).
fn write_secret(path: &Path, payload: &str) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {} for write", path.display()))?;
    file.write_all(payload.as_bytes())
        .with_context(|| format!("write to {}", path.display()))?;
    Ok(())
}

/// RAII tempdir guard. Removes the directory (and any contents — including
/// captured credentials) on drop, whether the caller exits via Ok, Err, or
/// panic. This is the primary defense against leaving credentials on disk
/// when something fails between login and keychain save.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create(prefix: &str) -> Result<Self> {
        // We avoid the `tempfile` crate to keep the dep tree minimal.
        // SystemTime's nanos-since-epoch is unique enough for our single-
        // process, sequential use case.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("{prefix}-{nanos:x}"));
        fs::create_dir(&dir).with_context(|| format!("create tempdir {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {} 0700", dir.display()))?;
        Ok(Self { path: dir })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best-effort: nothing useful to do if removal fails (process is
        // exiting). The OS will reclaim /tmp eventually either way.
        let _ = fs::remove_dir_all(&self.path);
    }
}
