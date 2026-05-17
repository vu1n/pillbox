//! Per-agent adapters.
//!
//! Each agent is described by a small data struct ([`AgentSpec`]) — the
//! provider id, the guest paths it reads/writes, the argv to invoke for
//! login vs run, and an optional OAuth callback port to forward. The
//! login + run flows are generic over that spec; per-agent files are
//! reserved for adapters that grow agent-specific quirks beyond the
//! spec (none yet).

use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};

use crate::{docker, keychain};

const GUEST_HOME: &str = "/home/lum";
const GUEST_WORKSPACE: &str = "/workspace";

/// Static description of a coding-agent adapter. Adding a new agent =
/// adding one `AgentSpec` constant below.
#[derive(Clone, Copy)]
pub struct AgentSpec {
    /// Provider id — keychain account name + CLI subject (e.g. `claude`).
    pub id: &'static str,
    /// Guest directory the agent writes its credentials into. Pillbox
    /// bind-mounts a host tempdir on top of this path during both login
    /// and run so it can stage / capture the credentials file.
    pub guest_cred_dir: &'static str,
    /// Filename within `guest_cred_dir` that holds the credentials.
    pub cred_filename: &'static str,
    /// argv for the login flow (runs after the standard `docker run`
    /// flags + image name).
    pub login_argv: &'static [&'static str],
    /// argv prefix for the run flow. User-supplied args are appended.
    pub run_argv: &'static [&'static str],
    /// Optional OAuth callback port to forward host:port → container:port
    /// during login. `None` = no port forward (device-code flow or
    /// agents that don't use browser callback).
    pub oauth_port: Option<u16>,
}

pub const CLAUDE: AgentSpec = AgentSpec {
    id: "claude",
    guest_cred_dir: "/home/lum/.claude",
    cred_filename: ".credentials.json",
    login_argv: &["claude", "auth", "login", "--claudeai"],
    run_argv: &["claude"],
    oauth_port: Some(54545),
};

pub const CODEX: AgentSpec = AgentSpec {
    id: "codex",
    guest_cred_dir: "/home/lum/.codex",
    cred_filename: "auth.json",
    login_argv: &["codex", "login"],
    run_argv: &["codex"],
    // codex's flow appears to use a different mechanism than claude's —
    // leaving port unmapped for v0.2 and we'll add if the OAuth flow
    // turns out to need one.
    oauth_port: None,
};

impl AgentSpec {
    pub fn login(&self) -> Result<()> {
        docker::check_ready()?;

        let tmp = TempDir::create(&format!("pillbox-{}-login", self.id))?;
        let mount = format!("{}:{}", tmp.path().display(), self.guest_cred_dir);

        let mut args = base_docker_args();
        if let Some(port) = self.oauth_port {
            args.push("-p".into());
            args.push(format!("{port}:{port}"));
        }
        args.push("-v".into());
        args.push(mount);
        args.push(docker::RUNNER_IMAGE.into());
        args.extend(self.login_argv.iter().map(|s| s.to_string()));

        println!("pillbox: starting {} login inside a sandbox.", self.id);
        println!("pillbox: follow the prompts (and any URL) printed by the sandbox below.");
        println!();

        let status = docker::run_interactive(&args)?;
        if !status.success() {
            return Err(anyhow!(
                "{} login exited with status {status}. Re-run `pillbox {} login`.",
                self.id, self.id
            ));
        }

        let creds_path = tmp.path().join(self.cred_filename);
        let payload = match fs::read_to_string(&creds_path) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(anyhow!(
                    "{} login completed but no credentials file at {}.\n\
                     Check the sandbox output above for hints.",
                    self.id,
                    creds_path.display()
                ));
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", creds_path.display())),
        };

        keychain::save(self.id, &payload)?;
        drop(tmp);

        println!();
        println!(
            "pillbox: ✓ credentials stored in your OS keychain (service `pillbox`, account `{}`).",
            self.id
        );
        println!("pillbox: try `pillbox {} run` to launch it in a sandboxed shell.", self.id);
        Ok(())
    }

    pub fn run(&self, extra_args: Vec<String>) -> Result<()> {
        docker::check_ready()?;

        let payload = keychain::load(self.id)?.ok_or_else(|| {
            anyhow!(
                "no stored credentials for `{}`. Run `pillbox {} login` first.",
                self.id, self.id
            )
        })?;

        let tmp = TempDir::create(&format!("pillbox-{}-creds", self.id))?;
        let creds_path = tmp.path().join(self.cred_filename);
        write_secret(&creds_path, &payload)?;

        let cwd = std::env::current_dir().context("resolve current working directory")?;
        let mut args = base_docker_args();
        args.extend([
            "-v".into(),
            format!("{}:{}", tmp.path().display(), self.guest_cred_dir),
            "-v".into(),
            format!("{}:{GUEST_WORKSPACE}", cwd.display()),
            "-w".into(),
            GUEST_WORKSPACE.into(),
            docker::RUNNER_IMAGE.into(),
        ]);
        args.extend(self.run_argv.iter().map(|s| s.to_string()));
        args.extend(extra_args);

        let status = docker::run_interactive(&args)?;
        drop(tmp);
        if !status.success() {
            return Err(anyhow!("{} exited with status {status}", self.id));
        }
        Ok(())
    }
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
/// panic. Primary defense against leaving credentials on disk when
/// something fails between login and keychain save.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create(prefix: &str) -> Result<Self> {
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
        let _ = fs::remove_dir_all(&self.path);
    }
}
