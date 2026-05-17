//! Claude Code adapter.
//!
//! Login flow: bind-mount a host tempdir at `/home/lum/.claude`, run
//! `claude /login` inside the runner image, and after the container
//! exits read `<tempdir>/.credentials.json` and persist its contents
//! to the OS keychain under provider id `claude`.
//!
//! Run flow: load the stored credentials, write them to a host tempdir,
//! bind-mount that tempdir at `/home/lum/.claude`, bind-mount the current
//! working directory at `/workspace`, and exec `claude <args...>` inside
//! the runner image.

use std::{fs, path::PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::{docker, keychain};

const PROVIDER: &str = "claude";
const GUEST_HOME: &str = "/home/lum";
const GUEST_CLAUDE_DIR: &str = "/home/lum/.claude";
const GUEST_WORKSPACE: &str = "/workspace";

/// Port that Claude Code's OAuth callback listener uses. If wrong we'll
/// need to detect dynamically from claude's stdout in v0.2.
///
/// TODO(v0.1): verify on a real `claude /login` invocation. If claude
/// picks a random port, we'll fall back to `-P` (publish all) and
/// document a known-issue.
const CLAUDE_OAUTH_PORT: u16 = 54545;

pub fn login() -> Result<()> {
    docker::check_available()?;
    docker::check_image()?;

    let tmp = tempdir("pillbox-claude-login")?;
    let mount_src = tmp.to_string_lossy().to_string();

    println!("pillbox: starting Claude Code OAuth flow inside a sandbox.");
    println!("pillbox: a URL will print below — open it in your browser to authenticate.");
    println!();

    // Bind-mount tempdir at /home/lum/.claude so claude writes
    // .credentials.json into a location we can read after exit. The
    // container runs as root (default) and the tempdir is owned by the
    // host user — Docker Desktop's macOS userns mapping makes that work.
    let port_map = format!("{CLAUDE_OAUTH_PORT}:{CLAUDE_OAUTH_PORT}");
    let mount_arg = format!("{mount_src}:{GUEST_CLAUDE_DIR}");
    let home_env = format!("HOME={GUEST_HOME}");

    let status = docker::run_interactive(&[
        "-it",
        "--rm",
        "-p",
        &port_map,
        "-v",
        &mount_arg,
        "-e",
        &home_env,
        "-e",
        "TERM=xterm-256color",
        docker::RUNNER_IMAGE,
        "claude",
        "/login",
    ])?;

    if !status.success() {
        return Err(anyhow!(
            "claude /login exited with status {status}. \
             Re-run `pillbox claude login` and complete the OAuth flow."
        ));
    }

    let creds_path = tmp.join(".credentials.json");
    if !creds_path.exists() {
        return Err(anyhow!(
            "claude /login completed but no credentials file was written at {}.\n\
             Check the sandbox output above for hints.",
            creds_path.display()
        ));
    }
    let payload = fs::read_to_string(&creds_path)
        .with_context(|| format!("read {}", creds_path.display()))?;

    keychain::save(PROVIDER, &payload)?;

    // Best-effort cleanup. macOS tempdirs auto-clear, but the credentials
    // file deserves an explicit unlink to minimize on-disk lifetime.
    let _ = fs::remove_file(&creds_path);
    let _ = fs::remove_dir_all(&tmp);

    println!();
    println!("pillbox: ✓ credentials stored in your OS keychain (service `pillbox`, account `claude`).");
    println!("pillbox: try `pillbox claude run` to launch claude in a sandboxed shell.");
    Ok(())
}

pub fn run(args: Vec<String>) -> Result<()> {
    docker::check_available()?;
    docker::check_image()?;

    let payload = keychain::load(PROVIDER)?.ok_or_else(|| {
        anyhow!(
            "no stored credentials for `claude`. Run `pillbox claude login` first."
        )
    })?;

    // Stage creds into a tempdir we bind-mount into the sandbox.
    let creds_tmp = tempdir("pillbox-claude-creds")?;
    let creds_path = creds_tmp.join(".credentials.json");
    fs::write(&creds_path, &payload).with_context(|| {
        format!("write staged credentials to {}", creds_path.display())
    })?;
    set_secret_perms(&creds_path)?;

    // Stage cwd → /workspace.
    let cwd = std::env::current_dir().context("resolve current working directory")?;
    let cwd_str = cwd.to_string_lossy().to_string();
    let creds_mount = format!("{}:{GUEST_CLAUDE_DIR}", creds_tmp.to_string_lossy());
    let cwd_mount = format!("{cwd_str}:{GUEST_WORKSPACE}");
    let home_env = format!("HOME={GUEST_HOME}");

    let mut docker_args = vec![
        "-it",
        "--rm",
        "-v",
        &creds_mount,
        "-v",
        &cwd_mount,
        "-w",
        GUEST_WORKSPACE,
        "-e",
        &home_env,
        "-e",
        "TERM=xterm-256color",
        docker::RUNNER_IMAGE,
        "claude",
    ];
    let extras: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    docker_args.extend(extras);

    let status = docker::run_interactive(&docker_args)?;

    // TODO(v0.2): if claude refreshed the access token mid-session,
    //             re-persist the updated .credentials.json back to the
    //             keychain so we don't drift from the live token.

    let _ = fs::remove_file(&creds_path);
    let _ = fs::remove_dir_all(&creds_tmp);

    if !status.success() {
        return Err(anyhow!("claude exited with status {status}"));
    }
    Ok(())
}

/// Make a fresh tempdir with a stable prefix so users can find it in
/// `/tmp` if something goes wrong mid-flow. We avoid the `tempfile`
/// crate to keep pillbox's dep tree minimal.
fn tempdir(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let unique = format!(
        "{prefix}-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%f")
    );
    let dir = base.join(unique);
    fs::create_dir_all(&dir).with_context(|| format!("create tempdir {}", dir.display()))?;
    set_secret_perms(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn set_secret_perms(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {} {:o}", path.display(), mode))
}

#[cfg(not(unix))]
fn set_secret_perms(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
