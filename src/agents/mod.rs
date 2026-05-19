//! Per-agent adapters.
//!
//! v0.6: each agent's persistent HOME ("auth state") lives under the
//! resolved auth pillbox. PR 2 always resolves to the **global** pillbox
//! — one `claude login` is shared across every project pillbox. v0.7 may
//! expose a per-project auth override if real signal materializes.
//!
//! Storage shape: `<auth_pillbox>/auth/<provider>/`. That directory is
//! bind-mounted at `/home/lum` (the guest's HOME) for both login and run.
//! Whatever the agent writes — `.credentials.json`, settings, refresh
//! tokens — persists there naturally.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};

use crate::pillbox::{self, Pillbox};
use crate::{docker, errors::PillboxError};

pub(crate) const GUEST_HOME: &str = "/home/lum";
pub(crate) const GUEST_WORKSPACE: &str = "/workspace";

#[derive(Clone, Copy)]
pub struct AgentSpec {
    pub(crate) id: &'static str,
    pub(crate) cred_sentinel: &'static str,
    pub(crate) login_argv: &'static [&'static str],
    pub(crate) run_argv: &'static [&'static str],
    pub(crate) oauth_port: Option<u16>,
    pub(crate) post_login_finalize: Option<fn(&Path) -> Result<()>>,
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

pub const CODEX: AgentSpec = AgentSpec {
    id: "codex",
    cred_sentinel: ".codex/auth.json",
    login_argv: &["codex", "login", "--device-auth"],
    run_argv: &["codex"],
    oauth_port: None,
    post_login_finalize: None,
    vault_capable: true,
};

pub const ALL: &[&AgentSpec] = &[&CLAUDE, &CODEX];

fn finalize_claude_onboarding(home: &Path) -> Result<()> {
    let path = home.join(".claude.json");
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("expected top-level JSON object in {}", path.display()))?;
    obj.insert(
        "hasCompletedOnboarding".to_string(),
        serde_json::Value::Bool(true),
    );
    let serialized = serde_json::to_string_pretty(&value)
        .with_context(|| format!("serialize {}", path.display()))?;
    fs::write(&path, serialized).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

impl AgentSpec {
    pub(crate) fn id(&self) -> &'static str {
        self.id
    }

    /// Resolve the auth pillbox for this agent. PR 2: always global. PR 3+
    /// may consult the resolved pillbox's `[auth]` config to opt into a
    /// per-project override.
    pub(crate) fn auth_pillbox(&self, _resolved: &Pillbox) -> Pillbox {
        pillbox::global()
    }

    /// `<auth_pillbox>/auth/<id>/` — created on first use, 0700.
    pub(crate) fn home_dir(&self, resolved: &Pillbox) -> Result<PathBuf> {
        let auth = self.auth_pillbox(resolved);
        let auth_root = auth.subdir("auth")?;
        let dir = auth_root.join(self.id);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {} 0700", dir.display()))?;
        Ok(dir)
    }

    pub(crate) fn is_authenticated(&self, resolved: &Pillbox) -> bool {
        match self.home_dir(resolved) {
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

    pub(crate) fn login(&self, resolved: &Pillbox) -> Result<()> {
        docker::check_ready()?;

        let home = self.home_dir(resolved)?;

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
            .with_next(format!("pillbox auth login --agent {}", self.id))
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
            .with_next(format!(
                "pillbox auth login --agent {}   # check the sandbox output above for clues",
                self.id
            ))
            .into());
        }

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
            "pillbox: try `pillbox run --agent {}` to launch it.",
            self.id
        );
        Ok(())
    }

    pub(crate) fn forget(&self, resolved: &Pillbox) -> Result<bool> {
        let home = self.home_dir(resolved)?;
        if !home.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&home).with_context(|| format!("remove {}", home.display()))?;
        Ok(true)
    }
}

pub(crate) struct RunOpts {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) name: Option<String>,
    pub(crate) mounts: Vec<String>,
    /// `--with NAME[=ENV_VAR]` entries (parsed from raw CLI strings)
    pub(crate) withs: Vec<String>,
    /// `--env BUNDLE` names
    pub(crate) env_bundles: Vec<String>,
    /// `--env-file PATH` paths (ad-hoc, no persistence)
    pub(crate) env_files: Vec<PathBuf>,
    /// `--vault` — route API traffic through the pillbox stub-swap proxy.
    pub(crate) vault: bool,
    pub(crate) args: Vec<String>,
}

#[allow(dead_code)]
impl RunOpts {
    /// In v0.6 PR 2, the only pillbox.toml field that backs a RunOpts
    /// default is `name`. Multi-value defaults from v0.5 (`with`, `mount`,
    /// `env_file`, `env`) are dropped — they didn't carry their weight
    /// and made the descriptor sprawl.
    pub(crate) fn apply_defaults(&mut self, cfg: crate::config::Config) {
        if self.name.is_none() {
            self.name = cfg.name;
        }
    }
}

pub(crate) struct ResolvedWith {
    pub(crate) secret_name: String,
    pub(crate) env_var: String,
    pub(crate) meta: Option<crate::vault::VaultMeta>,
    pub(crate) raw_entry: String,
}

pub(crate) fn resolve_with_entries(
    resolved: &Pillbox,
    withs: &[String],
) -> Result<Vec<ResolvedWith>> {
    let mut out = Vec::with_capacity(withs.len());
    for entry in withs {
        let (secret_name, env_var) = match entry.split_once('=') {
            Some((s, e)) => (s.to_string(), e.to_string()),
            None => (entry.clone(), entry.clone()),
        };
        let meta = crate::secrets::read_meta(resolved, &secret_name)?;
        out.push(ResolvedWith {
            secret_name,
            env_var,
            meta,
            raw_entry: entry.clone(),
        });
    }
    Ok(out)
}

pub(crate) fn resolve_run_env(
    resolved: &Pillbox,
    opts: &RunOpts,
    withs: &[ResolvedWith],
    mut vault: Option<&mut crate::vault::VaultSession>,
) -> Result<std::collections::BTreeMap<String, String>> {
    use std::collections::BTreeMap;
    let mut env: BTreeMap<String, String> = BTreeMap::new();

    // Layer 1: stored env bundles (inheritance applies).
    for bundle_name in &opts.env_bundles {
        let vars = crate::envs::read(resolved, bundle_name)?.ok_or_else(|| {
            PillboxError::runtime("run", format!("env bundle `{bundle_name}` not found"))
                .with_next("pillbox env list  # see what's stored".to_string())
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

    // Layer 3: --with entries.
    for w in withs {
        let real_value = crate::secrets::read(resolved, &w.secret_name)?.ok_or_else(|| {
            PillboxError::runtime("run", format!("secret `{}` not found", w.secret_name))
                .with_next(format!("pillbox secret add {}", w.secret_name))
        })?;

        let injected = match (w.meta.as_ref(), vault.as_deref_mut()) {
            (Some(meta), Some(session)) => {
                session.lease_api_key(&w.secret_name, real_value.trim_end(), meta)?
            }
            (Some(_meta), None) => {
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
            eprintln!(
                "pillbox: note: {} shadowed by --with {}",
                w.env_var, w.raw_entry
            );
        }
    }

    Ok(env)
}

pub(crate) fn base_docker_args() -> Vec<String> {
    vec![
        "-it".into(),
        "--rm".into(),
        "-e".into(),
        format!("HOME={GUEST_HOME}"),
        "-e".into(),
        "TERM=xterm-256color".into(),
        "-e".into(),
        format!("PATH=/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
    ]
}

pub(crate) fn workspace_mount_name(host: &Path, override_name: Option<&str>) -> Result<String> {
    if let Some(name) = override_name {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(PillboxError::usage(
                "run",
                format!(
                    "--name `{name}` must be a non-empty single path component (no `/` or NUL)"
                ),
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
    use crate::pillbox;
    use crate::secrets::{AddSource, WriteScope};
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
            args: Vec::new(),
        }
    }

    #[test]
    fn resolve_run_env_injects_plain_secret_without_vault_session() {
        with_isolated_home("agents-plain", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_PLAIN", "raw-value");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "PLAIN_KEY",
                AddSource::EnvVar("__PILLBOX_TEST_PLAIN".into()),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_PLAIN");

            let opts = run_opts_with_withs(vec!["PLAIN_KEY"]);
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let env = resolve_run_env(&g, &opts, &withs, None).unwrap();
            assert_eq!(env.get("PLAIN_KEY").map(String::as_str), Some("raw-value"));
        });
    }

    #[test]
    fn resolve_run_env_swaps_vaulted_secret_for_stub() {
        with_isolated_home("agents-vault", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_VAULTED", "sk-ant-api03-REAL");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "VAULTED_KEY",
                AddSource::EnvVar("__PILLBOX_TEST_VAULTED".into()),
                false,
                Some(VaultMeta::new(
                    "api.anthropic.com".into(),
                    HeaderScheme::XApiKey,
                    "sk-ant-api03-".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_VAULTED");

            let mut session = VaultSession::start(None::<OAuthAgent>, &g).unwrap();
            let opts = run_opts_with_withs(vec!["VAULTED_KEY"]);
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let env = resolve_run_env(&g, &opts, &withs, Some(&mut session)).unwrap();

            let injected = env.get("VAULTED_KEY").cloned().unwrap();
            assert!(injected.starts_with("sk-ant-api03-"), "got {injected}");
            assert_ne!(injected, "sk-ant-api03-REAL");
        });
    }

    #[test]
    fn resolve_run_env_mixed_vaulted_and_plain() {
        with_isolated_home("agents-mix", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_MIX_P", "plain-val");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "MIX_PLAIN",
                AddSource::EnvVar("__PILLBOX_TEST_MIX_P".into()),
                false,
                None,
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_MIX_P");
            std::env::set_var("__PILLBOX_TEST_MIX_V", "ghp_REAL_token");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "MIX_VAULTED",
                AddSource::EnvVar("__PILLBOX_TEST_MIX_V".into()),
                false,
                Some(VaultMeta::new(
                    "api.github.com".into(),
                    HeaderScheme::AuthorizationBearer,
                    "ghp_".into(),
                )),
            )
            .unwrap();
            std::env::remove_var("__PILLBOX_TEST_MIX_V");

            let mut session = VaultSession::start(None::<OAuthAgent>, &g).unwrap();
            let opts = run_opts_with_withs(vec!["MIX_PLAIN", "MIX_VAULTED=GITHUB_TOKEN"]);
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let env = resolve_run_env(&g, &opts, &withs, Some(&mut session)).unwrap();

            assert_eq!(env.get("MIX_PLAIN").map(String::as_str), Some("plain-val"));
            let stub = env.get("GITHUB_TOKEN").cloned().unwrap();
            assert!(stub.starts_with("ghp_"));
            assert_ne!(stub, "ghp_REAL_token");
        });
    }

    #[test]
    fn resolve_run_env_vaulted_without_session_errors_loudly() {
        with_isolated_home("agents-novault", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_NOV", "REAL");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "NO_SESSION_KEY",
                AddSource::EnvVar("__PILLBOX_TEST_NOV".into()),
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
            let withs = resolve_with_entries(&g, &opts.withs).unwrap();
            let err = resolve_run_env(&g, &opts, &withs, None).unwrap_err();
            let s = format!("{err}");
            assert!(s.contains("vaulted"), "expected vaulted error, got: {s}");
        });
    }

    #[test]
    fn resolve_with_entries_detects_meta_sidecar() {
        with_isolated_home("agents-detect", || {
            let g = pillbox::global();
            std::env::set_var("__PILLBOX_TEST_DETECT_A", "v");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "DETECT_PLAIN",
                AddSource::EnvVar("__PILLBOX_TEST_DETECT_A".into()),
                false,
                None,
            )
            .unwrap();
            std::env::set_var("__PILLBOX_TEST_DETECT_B", "v");
            crate::secrets::add(
                &g,
                WriteScope::Resolved,
                "DETECT_VAULTED",
                AddSource::EnvVar("__PILLBOX_TEST_DETECT_B".into()),
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

            let plain = resolve_with_entries(&g, &["DETECT_PLAIN".into()]).unwrap();
            assert!(plain[0].meta.is_none());

            let vaulted = resolve_with_entries(&g, &["DETECT_VAULTED".into()]).unwrap();
            assert!(vaulted[0].meta.is_some());
        });
    }

    #[test]
    fn home_dir_is_under_global_auth() {
        with_isolated_home("agents-global-auth", || {
            // Construct a project pillbox; the auth scope should still
            // resolve to global per the v0.6 rule.
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(Some("p".into()), None).unwrap();
            let proj = crate::pillbox::Pillbox::resolve(None).unwrap();
            assert!(!proj.is_global());

            let home = CLAUDE.home_dir(&proj).unwrap();
            // home is <home>/.pillbox/global/auth/claude
            let s = home.display().to_string();
            assert!(s.contains(".pillbox/global/auth/claude"), "got: {s}");

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }
}
