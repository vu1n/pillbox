//! Per-agent adapters.
//!
//! Each agent gets a persistent HOME directory at
//! `~/.pillbox/data/<provider>/` on the host. That directory is
//! bind-mounted at `/home/lum` (the guest's HOME) for both login and
//! run. Whatever the agent writes — `.credentials.json`, `.claude.json`,
//! `.codex/auth.json`, settings, refresh tokens — persists there
//! naturally. No tempdir capture/restore, no JSON bundling, no
//! keychain dance.
//!
//! Tradeoff vs. an OS-keychain-based approach: auth state lives as
//! plain files under `~/.pillbox/`, readable by anyone with access to
//! the user's home directory. For laptop dev use that's the same
//! posture as `~/.aws/credentials` or `~/.docker/config.json` — fine.
//! For shared / hostile-tenant scenarios it's not.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};

use crate::docker;

const GUEST_HOME: &str = "/home/lum";
const GUEST_WORKSPACE: &str = "/workspace";

#[derive(Clone, Copy)]
pub struct AgentSpec {
    /// Provider id — CLI subject + data directory name (e.g. `claude`).
    pub(crate) id: &'static str,
    /// File (relative to HOME) that must exist after login for it to be
    /// considered successful.
    pub(crate) cred_sentinel: &'static str,
    /// argv for the login flow.
    pub(crate) login_argv: &'static [&'static str],
    /// argv prefix for the run flow. User args are appended.
    pub(crate) run_argv: &'static [&'static str],
    /// OAuth callback port the agent's login server binds. `None` for
    /// device-code flows. Override with `PILLBOX_<ID>_OAUTH_PORT`.
    pub(crate) oauth_port: Option<u16>,
}

pub const CLAUDE: AgentSpec = AgentSpec {
    id: "claude",
    cred_sentinel: ".claude/.credentials.json",
    login_argv: &["claude", "auth", "login", "--claudeai"],
    run_argv: &["claude"],
    oauth_port: Some(54545),
};

// codex's default login binds a localhost callback on a port it picks
// (observed: 1455) which the sandbox can't expose. --device-auth is
// codex's headless mode: URL + code in the terminal, user pastes in
// browser, codex polls. No port forward.
pub const CODEX: AgentSpec = AgentSpec {
    id: "codex",
    cred_sentinel: ".codex/auth.json",
    login_argv: &["codex", "login", "--device-auth"],
    run_argv: &["codex"],
    oauth_port: None,
};

pub const ALL: &[&AgentSpec] = &[&CLAUDE, &CODEX];

impl AgentSpec {
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// `~/.pillbox/data/<id>/` — created on first use.
    pub fn home_dir(&self) -> Result<PathBuf> {
        let base = dirs::home_dir()
            .context("could not resolve $HOME")?
            .join(".pillbox")
            .join("data")
            .join(self.id);
        Ok(base)
    }

    /// Whether the agent has been logged in (cred sentinel present).
    pub fn is_authenticated(&self) -> bool {
        match self.home_dir() {
            Ok(home) => home.join(self.cred_sentinel).exists(),
            Err(_) => false,
        }
    }

    fn resolved_oauth_port(&self) -> Option<u16> {
        let default = self.oauth_port?;
        let var = format!("PILLBOX_{}_OAUTH_PORT", self.id.to_uppercase());
        let port = std::env::var(&var)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default);
        Some(port)
    }

    pub fn login(&self) -> Result<()> {
        docker::check_ready()?;

        let home = ensure_provider_home(self)?;

        let mut args = base_docker_args();
        if let Some(port) = self.resolved_oauth_port() {
            args.push("-p".into());
            args.push(format!("{port}:{port}"));
        }
        args.push("-v".into());
        args.push(format!("{}:{GUEST_HOME}", home.display()));
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

        if !home.join(self.cred_sentinel).exists() {
            return Err(anyhow!(
                "{} login completed but `{}` was not written.\n\
                 Check the sandbox output above for hints.",
                self.id,
                home.join(self.cred_sentinel).display()
            ));
        }

        // Mark provider-specific "I've onboarded the user" flags so the
        // agent's interactive TUI doesn't re-prompt onboarding on every
        // run. Skipped silently if the agent doesn't need any.
        if let Err(e) = self.post_login_finalize(&home) {
            eprintln!(
                "pillbox: warning: could not finalize {} onboarding flags: {e}",
                self.id
            );
        }

        println!();
        println!(
            "pillbox: ✓ {} authenticated. State persisted at {}.",
            self.id,
            home.display()
        );
        println!(
            "pillbox: try `pillbox {} run` to launch it in a sandboxed shell.",
            self.id
        );
        Ok(())
    }

    pub fn run(&self, opts: RunOpts) -> Result<()> {
        docker::check_ready()?;

        let home = self.home_dir()?;
        if !home.join(self.cred_sentinel).exists() {
            return Err(anyhow!(
                "no stored credentials for `{}`. Run `pillbox {} login` first.",
                self.id, self.id
            ));
        }

        let workspace_host = match opts.workspace {
            Some(p) => p,
            None => std::env::current_dir().context("resolve current working directory")?,
        };
        let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
        let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");

        let mut args = base_docker_args();
        args.extend([
            "-v".into(),
            format!("{}:{GUEST_HOME}", home.display()),
            "-v".into(),
            format!("{}:{guest_workspace}", workspace_host.display()),
            "-w".into(),
            guest_workspace,
        ]);
        for m in &opts.mounts {
            args.push("-v".into());
            args.push(m.clone());
        }
        args.push(docker::RUNNER_IMAGE.into());
        args.extend(self.run_argv.iter().map(|s| s.to_string()));
        args.extend(opts.args);

        let status = docker::run_interactive(&args)?;
        if !status.success() {
            return Err(anyhow!("{} exited with status {status}", self.id));
        }
        Ok(())
    }

    /// Per-agent post-login fix-ups. Claude's interactive TUI re-runs
    /// its first-launch wizard ("Choose theme", "Select login method")
    /// every session unless `~/.claude.json` has `hasCompletedOnboarding:
    /// true`. The login flow doesn't set it — it's only set by clicking
    /// through the wizard end-to-end. We set it directly after a
    /// successful `claude auth login` so the next `pillbox claude run`
    /// drops straight into a working REPL.
    fn post_login_finalize(&self, home: &Path) -> Result<()> {
        if self.id != "claude" {
            return Ok(());
        }
        let claude_json = home.join(".claude.json");
        if !claude_json.exists() {
            // claude wrote no profile file — nothing to mark. Shouldn't
            // happen since claude auth login always writes it, but be
            // defensive.
            return Ok(());
        }
        // Use jq inside a one-shot container — already in the image, no
        // host Python/jq required. The edit is atomic via mv.
        let one_liner =
            "jq '.hasCompletedOnboarding = true' /home/lum/.claude.json > /tmp/x && \
             mv /tmp/x /home/lum/.claude.json";
        let args = [
            "--rm".to_string(),
            "-v".into(),
            format!("{}:{GUEST_HOME}", home.display()),
            "-e".into(),
            format!("HOME={GUEST_HOME}"),
            docker::RUNNER_IMAGE.into(),
            "sh".into(),
            "-c".into(),
            one_liner.to_string(),
        ];
        let status = docker::run_interactive(&args)?;
        if !status.success() {
            return Err(anyhow!(
                "post-login finalize (jq edit of .claude.json) failed: {status}"
            ));
        }
        Ok(())
    }

    /// Wipe the provider's persistent state. Used by `pillbox auth rm`.
    pub fn forget(&self) -> Result<bool> {
        let home = self.home_dir()?;
        if !home.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&home)
            .with_context(|| format!("remove {}", home.display()))?;
        Ok(true)
    }
}

pub struct RunOpts {
    pub workspace: Option<PathBuf>,
    pub name: Option<String>,
    pub mounts: Vec<String>,
    pub args: Vec<String>,
}

/// Ensure `~/.pillbox/data/<provider>/` exists with 0700 perms.
fn ensure_provider_home(spec: &AgentSpec) -> Result<PathBuf> {
    let home = spec.home_dir()?;
    fs::create_dir_all(&home)
        .with_context(|| format!("create {}", home.display()))?;
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {} 0700", home.display()))?;
    Ok(home)
}

fn base_docker_args() -> Vec<String> {
    vec![
        "-it".into(),
        "--rm".into(),
        "-e".into(),
        format!("HOME={GUEST_HOME}"),
        "-e".into(),
        "TERM=xterm-256color".into(),
        // Include $HOME/.local/bin so agents that self-update or look
        // for their own native install at the standard XDG-ish location
        // don't print "your PATH is missing ~/.local/bin" warnings.
        // /usr/local/bin still ships the actual baked-in binaries.
        "-e".into(),
        format!("PATH={GUEST_HOME}/.local/bin:/usr/local/bin:/usr/bin:/bin"),
    ]
}

/// Resolve the basename used as the workspace mount point. Override
/// > derived basename > "workspace" fallback.
fn workspace_mount_name(host: &Path, override_name: Option<&str>) -> Result<String> {
    if let Some(name) = override_name {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(anyhow!(
                "--name `{name}` must be a non-empty single path component (no `/` or NUL)"
            ));
        }
        return Ok(name.to_string());
    }
    let derived = host
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("workspace");
    Ok(derived.to_string())
}
