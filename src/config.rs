//! `pillbox.toml` — the v0.6 pillbox descriptor.
//!
//! A `pillbox.toml` at the project root marks a directory as a pillbox.
//! In PR 3 the descriptor grows a `[workspace]` table that selects a
//! rustic-backed snapshot store; vault config will follow later.
//!
//! ```toml
//! # required
//! name = "my-project"
//!
//! # optional — default agent for `pillbox run`
//! agent = "claude"          # or "codex" or "opencode"
//!
//! [workspace]
//! backend = "local"        # or "s3"
//! # s3-only:
//! # endpoint = "https://<acct>.r2.cloudflarestorage.com"
//! # region = "auto"
//! # bucket = "my-bucket"
//! # prefix = "pillbox/"
//! # access_key_env = "R2_ACCESS_KEY"
//! # secret_key_env = "R2_SECRET_KEY"
//! ```
//!
//! Discovery is the same shape as `.gitignore` / `Cargo.toml`: walk up
//! from cwd until a `pillbox.toml` is found.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::errors::PillboxError;

/// `[workspace]` table — selects the rustic backend variant for this
/// pillbox. PR 3 introduces two variants: `local` (default) and `s3`.
///
/// S3 credentials are referenced **indirectly** via env-var names so
/// the descriptor stays safe to check into git. The values land in the
/// process env at pillbox-creation time + at every push/pull.
///
/// We deliberately **don't** set `deny_unknown_fields` here: this
/// struct is embedded in `meta.json` (see [`crate::pillbox::ProjectMeta`])
/// which has a documented forward-compat contract — older binaries
/// must keep parsing meta.json after v0.7+ adds fields. The top-level
/// `Config` is stricter because it backs a user-edited `pillbox.toml`
/// where catching typos matters more than reading future descriptors.
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceConfig {
    /// `local` (default) or `s3`. Missing → `local` so existing
    /// descriptors written before PR 3 keep working.
    #[serde(default)]
    pub(crate) backend: Option<String>,
    #[serde(default)]
    pub(crate) endpoint: Option<String>,
    #[serde(default)]
    pub(crate) region: Option<String>,
    #[serde(default)]
    pub(crate) bucket: Option<String>,
    #[serde(default)]
    pub(crate) prefix: Option<String>,
    #[serde(default)]
    pub(crate) access_key_env: Option<String>,
    #[serde(default)]
    pub(crate) secret_key_env: Option<String>,
}

/// Normalized backend selector. Centralizes the `local` / `s3` /
/// unknown decision so callers `match` on a closed set rather than on
/// stringly-typed values. New variants land here first; serialization
/// (`as_str`) stays stable so `pillbox.toml` text doesn't drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendKind {
    Local,
    S3,
}

impl BackendKind {
    /// Wire-format name written into `pillbox.toml` + read back by older
    /// binaries. Keep in sync with the documented schema in
    /// `docs/config.md`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BackendKind::Local => "local",
            BackendKind::S3 => "s3",
        }
    }
}

impl WorkspaceConfig {
    /// Normalize the backend selector. Returns [`BackendKind::Local`]
    /// for missing / empty / unknown values so callers always see one
    /// of the two allowed variants. Validation happens at `pillbox new`
    /// time, not at every load — older binaries reading a future
    /// descriptor should degrade gracefully.
    pub(crate) fn backend_kind(&self) -> BackendKind {
        match self.backend.as_deref() {
            Some("s3") => BackendKind::S3,
            _ => BackendKind::Local,
        }
    }
}

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // `agent`, `workspace`, `source` consumed by PR 3 + later
pub(crate) struct Config {
    /// Display name for the pillbox. Required in well-formed configs but
    /// optional here so a half-written file still parses (the loader
    /// enforces presence — see `Config::load_from`).
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// Default agent for `pillbox run` (`claude` | `codex`). `None` falls
    /// back to a built-in default at run time.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// Workspace backend selector (`local` | `s3`). See [`WorkspaceConfig`].
    #[serde(default)]
    pub(crate) workspace: WorkspaceConfig,

    /// Path the config was loaded from. Useful for `info` output.
    #[serde(skip)]
    pub(crate) source: Option<PathBuf>,
}

#[allow(dead_code)] // `load_from` is exercised by tests + PR 3
impl Config {
    pub(crate) fn load_from(path: &Path) -> Result<Config> {
        let raw = fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                "config load",
                format!("could not read {}: {e}", path.display()),
            )
        })?;
        let mut cfg: Config = toml::from_str(&raw)
            .map_err(|e| PillboxError::config("config load", format!("{}: {e}", path.display())))?;
        if cfg.name.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err(PillboxError::config(
                "config load",
                format!("{}: missing required field `name`", path.display()),
            )
            .into());
        }
        cfg.source = Some(path.to_path_buf());
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_config(dir: &Path, body: &str) {
        let mut f = fs::File::create(dir.join("pillbox.toml")).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn load_accepts_minimal_config() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"alpha\"\n");
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("alpha"));
        assert_eq!(cfg.agent, None);
    }

    #[test]
    fn load_parses_agent_field() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"a\"\nagent = \"claude\"\n");
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn load_rejects_missing_name() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "agent = \"claude\"\n");
        let err = Config::load_from(&root.path().join("pillbox.toml")).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("name"));
    }

    #[test]
    fn load_rejects_unknown_field() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"x\"\nbogus = 1\n");
        let err = Config::load_from(&root.path().join("pillbox.toml")).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("bogus") || s.contains("unknown"));
    }

    #[test]
    fn load_rejects_legacy_with_field() {
        // v0.5 had `with = [...]`; v0.6 drops it. Old configs fail loud.
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            "name = \"x\"\nwith = [\"ANTHROPIC_API_KEY\"]\n",
        );
        let err = Config::load_from(&root.path().join("pillbox.toml")).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("with") || s.contains("unknown"));
    }

    #[test]
    fn load_accepts_empty_workspace_table() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"x\"\n\n[workspace]\n");
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        // Missing backend → defaults to local.
        assert_eq!(cfg.workspace.backend_kind(), BackendKind::Local);
    }

    #[test]
    fn load_parses_workspace_local_backend() {
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            "name = \"x\"\n\n[workspace]\nbackend = \"local\"\n",
        );
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.workspace.backend_kind(), BackendKind::Local);
    }

    #[test]
    fn load_parses_workspace_s3_backend() {
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            r#"name = "x"

[workspace]
backend = "s3"
endpoint = "https://acct.r2.cloudflarestorage.com"
region = "auto"
bucket = "my-bucket"
prefix = "pillbox/"
access_key_env = "R2_ACCESS_KEY"
secret_key_env = "R2_SECRET_KEY"
"#,
        );
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.workspace.backend_kind(), BackendKind::S3);
        assert_eq!(
            cfg.workspace.endpoint.as_deref(),
            Some("https://acct.r2.cloudflarestorage.com")
        );
        assert_eq!(cfg.workspace.bucket.as_deref(), Some("my-bucket"));
        assert_eq!(cfg.workspace.prefix.as_deref(), Some("pillbox/"));
        assert_eq!(
            cfg.workspace.access_key_env.as_deref(),
            Some("R2_ACCESS_KEY")
        );
    }

    #[test]
    fn workspace_backend_kind_normalizes_unknown_to_local() {
        let w = WorkspaceConfig {
            backend: Some("bogus".into()),
            ..Default::default()
        };
        assert_eq!(w.backend_kind(), BackendKind::Local);
        let w = WorkspaceConfig {
            backend: Some("s3".into()),
            ..Default::default()
        };
        assert_eq!(w.backend_kind(), BackendKind::S3);
        let w = WorkspaceConfig::default();
        assert_eq!(w.backend_kind(), BackendKind::Local);
    }
}
