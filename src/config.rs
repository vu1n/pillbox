//! `pillbox.toml` — the v0.6 pillbox descriptor.
//!
//! A `pillbox.toml` at the project root marks a directory as a pillbox.
//! The descriptor is intentionally minimal in PR 2 — just enough to give
//! the pillbox a display name and pick a default agent. Workspace
//! backends (PR 3) and vault config will add more sections later.
//!
//! ```toml
//! # required
//! name = "my-project"
//!
//! # optional — default agent for `pillbox run`
//! agent = "claude"          # or "codex"
//!
//! # PR 3 will fill this in.
//! [workspace]
//! ```
//!
//! Discovery is the same shape as `.gitignore` / `Cargo.toml`: walk up
//! from cwd until a `pillbox.toml` is found.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::Deserialize;

use crate::errors::PillboxError;

/// `[workspace]` table — empty in PR 2, scaffolding for the workspace
/// backends in PR 3.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // populated in PR 3 (workspace backends)
pub(crate) struct WorkspaceConfig {
    // Empty intentionally — backend / endpoint / bucket land in PR 3.
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
    /// Reserved for PR 3.
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
        Config::load_from(&root.path().join("pillbox.toml")).unwrap();
    }
}
