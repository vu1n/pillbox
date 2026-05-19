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

use crate::{docker, errors::PillboxError};

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
    /// Optional fix-up that runs on the *host* after a successful login.
    /// Agents whose interactive TUI needs setup-marker files (e.g. claude's
    /// `hasCompletedOnboarding`) wire one of these in. `None` means
    /// `claude auth login` (or equivalent) wrote everything the agent
    /// needs and no further fix-up is required.
    pub(crate) post_login_finalize: Option<fn(&Path) -> Result<()>>,
    /// Whether `pillbox <agent> run --vault` is supported. The vault
    /// provider for the agent is looked up by [`Self::id`] —
    /// `vault_capable = true` requires a matching entry in
    /// `vault::providers::registry()`.
    pub(crate) vault_capable: bool,
}

pub const CLAUDE: AgentSpec = AgentSpec {
    id: "claude",
    cred_sentinel: ".claude/.credentials.json",
    login_argv: &["claude", "auth", "login", "--claudeai"],
    run_argv: &["claude"],
    oauth_port: Some(54545),
    post_login_finalize: Some(finalize_claude_onboarding),
    vault_capable: true,
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
    post_login_finalize: None,
    // v0.5: vault supports codex ChatGPT-mode auth.json. ApiKey-mode
    // auth.json is rejected at lease time (see vault::providers::codex).
    vault_capable: true,
};

pub const ALL: &[&AgentSpec] = &[&CLAUDE, &CODEX];

/// Set `hasCompletedOnboarding: true` in `~/.pillbox/data/claude/.claude.json`
/// so claude's interactive TUI doesn't re-run its first-launch wizard
/// (theme picker, login-method picker) on every `pillbox claude run`.
/// The flag is normally set by clicking through the wizard end-to-end;
/// `claude auth login` itself doesn't set it.
fn finalize_claude_onboarding(home: &Path) -> Result<()> {
    let path = home.join(".claude.json");
    if !path.exists() {
        // Claude wrote no profile file. Shouldn't happen after a
        // successful auth login, but tolerate it rather than failing
        // the whole login flow.
        return Ok(());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("expected top-level JSON object in {}", path.display()))?;
    obj.insert(
        "hasCompletedOnboarding".to_string(),
        serde_json::Value::Bool(true),
    );
    let serialized = serde_json::to_string_pretty(&value)
        .with_context(|| format!("serialize {}", path.display()))?;
    fs::write(&path, serialized)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

impl AgentSpec {
    pub(crate) fn id(&self) -> &'static str {
        self.id
    }

    /// `~/.pillbox/data/<id>/` — created on first use.
    pub(crate) fn home_dir(&self) -> Result<PathBuf> {
        let home = std::env::var("HOME").context("could not resolve $HOME")?;
        Ok(PathBuf::from(home)
            .join(".pillbox")
            .join("data")
            .join(self.id))
    }

    /// Whether the agent has been logged in (cred sentinel present).
    pub(crate) fn is_authenticated(&self) -> bool {
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

    pub(crate) fn login(&self) -> Result<()> {
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
            return Err(PillboxError::runtime(
                "login",
                format!("{} exited with status {status}", self.id),
            )
            .with_next(format!("pillbox {} login", self.id))
            .into());
        }

        if !home.join(self.cred_sentinel).exists() {
            return Err(PillboxError::runtime(
                "login",
                format!(
                    "{} completed but `{}` was not written",
                    self.id,
                    home.join(self.cred_sentinel).display()
                ),
            )
            .with_next(format!("pillbox {} login   # check the sandbox output above for clues", self.id))
            .into());
        }

        // Apply the agent's post-login fix-up if it has one (e.g. claude
        // needs hasCompletedOnboarding: true so its TUI skips the wizard).
        // Warning-only — login itself already succeeded.
        if let Some(finalize) = self.post_login_finalize {
            if let Err(e) = finalize(&home) {
                eprintln!(
                    "pillbox: warning: could not finalize {} setup: {e}",
                    self.id
                );
            }
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

    pub(crate) fn run(&self, opts: RunOpts) -> Result<()> {
        docker::check_ready()?;

        let home = self.home_dir()?;
        if !home.join(self.cred_sentinel).exists() {
            return Err(PillboxError::runtime(
                "run",
                format!("no stored credentials for `{}`", self.id),
            )
            .with_next(format!("pillbox {} login", self.id))
            .into());
        }

        let workspace_host = match &opts.workspace {
            Some(p) => p.clone(),
            None => std::env::current_dir().context("resolve current working directory")?,
        };
        let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;
        let guest_workspace = format!("{GUEST_WORKSPACE}/{workspace_name}");

        if opts.vault && !self.vault_capable {
            return Err(PillboxError::usage(
                "run",
                format!("--vault is not supported for `{}`", self.id),
            )
            .into());
        }

        // Resolve `--with` entries up front (one sidecar lookup per entry,
        // shared across both the "needs session?" decision and the env
        // resolution below). This avoids walking the list twice with a
        // disk hit each time. The session may exist even when the agent
        // isn't using OAuth — non-vault-capable agents can still benefit
        // from API-key stub swap for `--with`'d secrets.
        let withs_resolved = resolve_with_entries(&opts.withs)?;
        let any_vaulted = withs_resolved.iter().any(|w| w.meta.is_some());
        let mut vault_session = if opts.vault || any_vaulted {
            let oauth = if opts.vault {
                Some(crate::vault::OAuthAgent {
                    agent_id: self.id,
                    agent_home: &home,
                })
            } else {
                None
            };
            Some(crate::vault::VaultSession::start(oauth)?)
        } else {
            None
        };

        let env_vars = resolve_run_env(&opts, &withs_resolved, vault_session.as_mut())?;

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
        for (k, v) in &env_vars {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        if let Some(session) = &vault_session {
            args.extend(session.docker_extras(GUEST_HOME));
            eprintln!(
                "pillbox: vault proxy listening on {} (ca: {})",
                session.listen_addr(),
                session.ca_cert_path().display()
            );
        }
        args.push(docker::RUNNER_IMAGE.into());
        args.extend(self.run_argv.iter().map(|s| s.to_string()));
        args.extend(opts.args);

        let status = docker::run_interactive(&args)?;
        drop(vault_session);
        if !status.success() {
            return Err(PillboxError::runtime(
                "run",
                format!("{} exited with status {status}", self.id),
            )
            .into());
        }
        Ok(())
    }

    /// Wipe the provider's persistent state. Used by `pillbox auth rm`.
    pub(crate) fn forget(&self) -> Result<bool> {
        let home = self.home_dir()?;
        if !home.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&home)
            .with_context(|| format!("remove {}", home.display()))?;
        Ok(true)
    }
}

pub(crate) struct RunOpts {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) name: Option<String>,
    pub(crate) mounts: Vec<String>,
    /// `--with NAME[=ENV_VAR]` entries (parsed from raw CLI strings)
    pub(crate) withs: Vec<String>,
    /// `--env BUNDLE` names (stored env bundles to inject)
    pub(crate) env_bundles: Vec<String>,
    /// `--env-file PATH` paths (ad-hoc .env files to inject, no persistence)
    pub(crate) env_files: Vec<PathBuf>,
    /// `--vault` — route API traffic through the pillbox stub-swap proxy.
    pub(crate) vault: bool,
    /// `--strict` — opt into the Gondolin microVM sandbox (gated until v0.6
    /// wires the spawn integration; today this is an error trigger).
    pub(crate) strict: bool,
    pub(crate) args: Vec<String>,
}

impl RunOpts {
    /// Layer `pillbox.toml` defaults under the CLI flags already present
    /// in `self`. Single-value fields (`name`) only fill in if CLI didn't
    /// supply one. Multi-value fields prepend the config's entries so CLI
    /// entries land later in the env composition order (higher precedence).
    pub(crate) fn apply_defaults(&mut self, cfg: crate::config::Config) {
        if self.name.is_none() {
            self.name = cfg.name;
        }
        prepend_vec(&mut self.mounts, cfg.mount);
        prepend_vec(&mut self.withs, cfg.with);
        if let Some(bundle) = cfg.env {
            self.env_bundles.insert(0, bundle);
        }
        prepend_vec(
            &mut self.env_files,
            cfg.env_file.into_iter().map(PathBuf::from).collect(),
        );
        // Bool fields OR-merge: either source can trigger, CLI can't unset
        // config-on.
        self.strict |= cfg.strict;
    }
}

fn prepend_vec<T>(dst: &mut Vec<T>, src: Vec<T>) {
    let cli = std::mem::take(dst);
    *dst = src;
    dst.extend(cli);
}

/// Compose the final env map applied to the run sandbox.
///
/// Precedence (later layers override earlier ones, per AGENTS.md):
///   1. `--env <bundle>` (lowest)
///   2. `--env-file <path>`
///   3. `--with NAME[=ENV_VAR]` (highest)
///
/// Emits one `pillbox: note: ENVVAR shadowed by --with` line to stderr
/// each time a higher-precedence layer overrides a lower one — visible
/// to agents without spamming.

/// One `--with` entry resolved against the secrets store. `meta` is
/// `Some` iff the secret has a `.meta.json` sidecar (i.e. it's vaulted).
/// Computed once per run so resolve_run_env doesn't re-stat per entry,
/// and so `run` can use the same data to decide whether to spin up a
/// `VaultSession` (any `meta.is_some()` → need session).
pub(crate) struct ResolvedWith {
    pub(crate) secret_name: String,
    pub(crate) env_var: String,
    pub(crate) meta: Option<crate::vault::VaultMeta>,
    pub(crate) raw_entry: String,
}

pub(crate) fn resolve_with_entries(withs: &[String]) -> Result<Vec<ResolvedWith>> {
    let mut out = Vec::with_capacity(withs.len());
    for entry in withs {
        let (secret_name, env_var) = match entry.split_once('=') {
            Some((s, e)) => (s.to_string(), e.to_string()),
            None => (entry.clone(), entry.clone()),
        };
        // Propagate sidecar parse errors: silently treating a vaulted
        // secret as plain would leak the real key into the guest.
        let meta = crate::secrets::read_meta(&secret_name)?;
        out.push(ResolvedWith {
            secret_name,
            env_var,
            meta,
            raw_entry: entry.clone(),
        });
    }
    Ok(out)
}

fn resolve_run_env(
    opts: &RunOpts,
    withs: &[ResolvedWith],
    mut vault: Option<&mut crate::vault::VaultSession>,
) -> Result<std::collections::BTreeMap<String, String>> {
    use std::collections::BTreeMap;
    let mut env: BTreeMap<String, String> = BTreeMap::new();

    // Layer 1: stored env bundles.
    for bundle_name in &opts.env_bundles {
        let vars = crate::envs::read(bundle_name)?.ok_or_else(|| {
            PillboxError::runtime(
                "run",
                format!("env bundle `{bundle_name}` not found"),
            )
            .with_next(format!("pillbox env list  # see what's stored"))
        })?;
        for (k, v) in vars {
            if let Some(prev) = env.insert(k.clone(), v) {
                eprintln!(
                    "pillbox: note: {k} shadowed by --env {bundle_name} (was set to `{}`)",
                    crate::secrets::mask(&prev)
                );
            }
        }
    }

    // Layer 2: ad-hoc .env files.
    for path in &opts.env_files {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                "run",
                format!("could not read --env-file {}: {e}", path.display()),
            )
        })?;
        let vars = crate::envs::parse_dotenv(&raw, &path.display().to_string())?;
        for (k, v) in vars {
            if let Some(_prev) = env.insert(k.clone(), v) {
                eprintln!(
                    "pillbox: note: {k} shadowed by --env-file {}",
                    path.display()
                );
            }
        }
    }

    // Layer 3: --with entries. The sidecar lookup already happened in
    // `resolve_with_entries`; here we only need to read the real value
    // and (if vaulted + session active) swap it for a stub. `opts.withs`
    // is the raw input — the resolved view is what we iterate.
    for w in withs {
        let real_value = crate::secrets::read(&w.secret_name)?.ok_or_else(|| {
            PillboxError::runtime(
                "run",
                format!("secret `{}` not found", w.secret_name),
            )
            .with_next(format!("pillbox secret add {}", w.secret_name))
        })?;

        let injected = match (w.meta.as_ref(), vault.as_deref_mut()) {
            (Some(meta), Some(session)) => {
                // Strip trailing whitespace on the real value before
                // handing it to the vault — same trim logic
                // secrets::read uses internally (defensive: real_value
                // already trimmed, but make explicit).
                session.lease_api_key(&w.secret_name, real_value.trim_end(), meta)?
            }
            (Some(_meta), None) => {
                // Vaulted secret but no session — shouldn't happen if
                // `run` initialised the session correctly. Fail loudly
                // rather than silently leaking the real key.
                return Err(PillboxError::runtime(
                    "run",
                    format!(
                        "secret `{}` is marked vaulted but no vault session is active",
                        w.secret_name
                    ),
                )
                .into());
            }
            (None, _) => real_value,
        };

        if let Some(_prev) = env.insert(w.env_var.clone(), injected) {
            eprintln!("pillbox: note: {} shadowed by --with {}", w.env_var, w.raw_entry);
        }
    }

    Ok(env)
}

/// Ensure `~/.pillbox/data/<provider>/` exists with 0700 perms.
fn ensure_provider_home(spec: &AgentSpec) -> Result<PathBuf> {
    let home = crate::paths::data_subdir("data")?.join(spec.id);
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
        // /usr/local/bin comes FIRST so the baked-in binaries win over
        // anything an agent might write into its own persistent
        // ~/.local/bin/ — defense against a future agent or its plugin
        // silently overriding the runtime image.
        "-e".into(),
        format!("PATH=/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
    ]
}

/// Resolve the basename used as the workspace mount point. Override
/// > derived basename > "workspace" fallback.
fn workspace_mount_name(host: &Path, override_name: Option<&str>) -> Result<String> {
    if let Some(name) = override_name {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(PillboxError::usage(
                "run",
                format!("--name `{name}` must be a non-empty single path component (no `/` or NUL)"),
            )
            .into());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::with_isolated_home;
    use crate::vault::{HeaderScheme, OAuthAgent, VaultMeta, VaultSession};

    fn run_opts_with_withs(withs: Vec<&str>) -> RunOpts {
        RunOpts {
            workspace: None,
            name: None,
            mounts: Vec::new(),
            withs: withs.into_iter().map(String::from).collect(),
            env_bundles: Vec::new(),
            env_files: Vec::new(),
            vault: false,
            strict: false,
            args: Vec::new(),
        }
    }

    #[test]
    fn resolve_run_env_injects_plain_secret_without_vault_session() {
        with_isolated_home("agents-plain", || {
            crate::secrets::add(
                "PLAIN_KEY",
                crate::secrets::AddSource::EnvVar({
                    std::env::set_var("__PILLBOX_TEST_PLAIN", "raw-value");
                    "__PILLBOX_TEST_PLAIN".into()
                }),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_PLAIN");

            let opts = run_opts_with_withs(vec!["PLAIN_KEY"]);
            let withs = resolve_with_entries(&opts.withs).unwrap();
            let env = resolve_run_env(&opts, &withs, None).unwrap();
            assert_eq!(env.get("PLAIN_KEY").map(String::as_str), Some("raw-value"));
        });
    }

    #[test]
    fn resolve_run_env_swaps_vaulted_secret_for_stub() {
        with_isolated_home("agents-vault", || {
            // Stash a vaulted secret. Meta points to api.anthropic.com which
            // is intercepted by the bundled AnthropicProvider.
            std::env::set_var("__PILLBOX_TEST_VAULTED", "sk-ant-api03-REAL");
            crate::secrets::add(
                "VAULTED_KEY",
                crate::secrets::AddSource::EnvVar("__PILLBOX_TEST_VAULTED".into()),
                false,
                Some(VaultMeta::new(
                    "api.anthropic.com".into(),
                    HeaderScheme::XApiKey,
                    "sk-ant-api03-".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_VAULTED");

            // No OAuth — just a session that supports API-key leases.
            let mut session = VaultSession::start(None::<OAuthAgent>).unwrap();
            let opts = run_opts_with_withs(vec!["VAULTED_KEY"]);
            let withs = resolve_with_entries(&opts.withs).unwrap();
            let env = resolve_run_env(&opts, &withs, Some(&mut session)).unwrap();

            let injected = env.get("VAULTED_KEY").cloned().unwrap();
            // Stub mimics the prefix, NOT the real value.
            assert!(injected.starts_with("sk-ant-api03-"), "got {injected}");
            assert_ne!(injected, "sk-ant-api03-REAL");
        });
    }

    #[test]
    fn resolve_run_env_mixed_vaulted_and_plain() {
        with_isolated_home("agents-mix", || {
            // Plain secret.
            std::env::set_var("__PILLBOX_TEST_MIX_P", "plain-val");
            crate::secrets::add(
                "MIX_PLAIN",
                crate::secrets::AddSource::EnvVar("__PILLBOX_TEST_MIX_P".into()),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_MIX_P");
            // Vaulted secret.
            std::env::set_var("__PILLBOX_TEST_MIX_V", "ghp_REAL_token");
            crate::secrets::add(
                "MIX_VAULTED",
                crate::secrets::AddSource::EnvVar("__PILLBOX_TEST_MIX_V".into()),
                false,
                Some(VaultMeta::new(
                    "api.github.com".into(),
                    HeaderScheme::AuthorizationBearer,
                    "ghp_".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_MIX_V");

            let mut session = VaultSession::start(None::<OAuthAgent>).unwrap();
            let opts = run_opts_with_withs(vec!["MIX_PLAIN", "MIX_VAULTED=GITHUB_TOKEN"]);
            let withs = resolve_with_entries(&opts.withs).unwrap();
            let env = resolve_run_env(&opts, &withs, Some(&mut session)).unwrap();

            assert_eq!(env.get("MIX_PLAIN").map(String::as_str), Some("plain-val"));
            let stub = env.get("GITHUB_TOKEN").cloned().unwrap();
            assert!(stub.starts_with("ghp_"));
            assert_ne!(stub, "ghp_REAL_token");
        });
    }

    #[test]
    fn resolve_run_env_vaulted_without_session_errors_loudly() {
        with_isolated_home("agents-novault", || {
            std::env::set_var("__PILLBOX_TEST_NOV", "REAL");
            crate::secrets::add(
                "NO_SESSION_KEY",
                crate::secrets::AddSource::EnvVar("__PILLBOX_TEST_NOV".into()),
                false,
                Some(VaultMeta::new(
                    "api.openai.com".into(),
                    HeaderScheme::AuthorizationBearer,
                    "sk-".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_NOV");

            let opts = run_opts_with_withs(vec!["NO_SESSION_KEY"]);
            let withs = resolve_with_entries(&opts.withs).unwrap();
            let err = resolve_run_env(&opts, &withs, None).unwrap_err();
            let s = format!("{err}");
            assert!(s.contains("vaulted"), "expected vaulted error, got: {s}");
        });
    }

    #[test]
    fn resolve_with_entries_detects_meta_sidecar() {
        with_isolated_home("agents-detect", || {
            std::env::set_var("__PILLBOX_TEST_DETECT_A", "v");
            crate::secrets::add(
                "DETECT_PLAIN",
                crate::secrets::AddSource::EnvVar("__PILLBOX_TEST_DETECT_A".into()),
                false,
                None,
            )
            .unwrap();
            std::env::set_var("__PILLBOX_TEST_DETECT_B", "v");
            crate::secrets::add(
                "DETECT_VAULTED",
                crate::secrets::AddSource::EnvVar("__PILLBOX_TEST_DETECT_B".into()),
                false,
                Some(VaultMeta::new(
                    "api.anthropic.com".into(),
                    HeaderScheme::XApiKey,
                    "sk-ant-api03-".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_DETECT_A");
            std::env::remove_var("__PILLBOX_TEST_DETECT_B");

            let plain = resolve_with_entries(&["DETECT_PLAIN".into()]).unwrap();
            assert!(plain[0].meta.is_none());

            let vaulted = resolve_with_entries(&["DETECT_VAULTED".into()]).unwrap();
            assert!(vaulted[0].meta.is_some());

            let mixed = resolve_with_entries(&[
                "DETECT_PLAIN".into(),
                "DETECT_VAULTED=GITHUB_TOKEN".into(),
            ])
            .unwrap();
            assert!(mixed.iter().any(|w| w.meta.is_some()));
        });
    }
}
