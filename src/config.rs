//! `pillbox.toml` — per-project defaults for `pillbox <agent> run`.
//!
//! Discovered by walking up from cwd to filesystem root, like `.gitignore`
//! or `Cargo.toml`. The first file found wins. Pass `--config PATH` to
//! point at a specific file, or `--no-config` to skip discovery.
//!
//! CLI flags layer on top of the file:
//! - single-value fields (`name`, `env`) — CLI overrides config
//! - multi-value fields (`with`, `mount`, `env_file`) — CLI appends to config
//!
//! Tilde-prefixed paths in the file are expanded against `$HOME`. CLI
//! flags don't need tilde handling because the shell already did it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::Deserialize;

use crate::errors::PillboxError;

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Override the workspace mount-point name (`/workspace/<name>`).
    pub(crate) name: Option<String>,
    /// Default `--env BUNDLE` to apply.
    pub(crate) env: Option<String>,
    /// Default `--with NAME[=ENV_VAR]` entries.
    #[serde(default)]
    pub(crate) with: Vec<String>,
    /// Default `--mount HOST:GUEST[:opts]` entries. Tilde-expanded.
    #[serde(default)]
    pub(crate) mount: Vec<String>,
    /// Default `--env-file PATH` entries. Tilde-expanded.
    #[serde(default)]
    pub(crate) env_file: Vec<String>,
    /// Path the config was loaded from. Useful for `--show-config` and error
    /// messages. Not in the TOML schema.
    #[serde(skip)]
    pub(crate) source: Option<PathBuf>,
}

impl Config {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Walk up from `start` looking for `pillbox.toml`. Returns the loaded
    /// config (with `source` set) or `None` if no file is found.
    pub(crate) fn discover_from(start: &Path) -> Result<Option<Config>> {
        let mut dir = start;
        loop {
            let candidate = dir.join("pillbox.toml");
            if candidate.is_file() {
                return Self::load_from(&candidate).map(Some);
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => return Ok(None),
            }
        }
    }

    /// Like `discover_from` but starts at `std::env::current_dir()`.
    pub(crate) fn discover() -> Result<Option<Config>> {
        let cwd = std::env::current_dir().map_err(|e| {
            PillboxError::runtime("config discover", format!("could not resolve cwd: {e}"))
        })?;
        Self::discover_from(&cwd)
    }

    /// Resolve the effective config for a `run` invocation given the CLI
    /// flags. Mirrors the user's expectation: `--no-config` wins, then
    /// `--config PATH`, otherwise discover-from-cwd (empty if nothing found).
    pub(crate) fn resolve(explicit: Option<PathBuf>, no_config: bool) -> Result<Config> {
        if no_config {
            return Ok(Self::empty());
        }
        if let Some(p) = explicit {
            return Self::load_from(&p);
        }
        Ok(Self::discover()?.unwrap_or_else(Self::empty))
    }

    pub(crate) fn load_from(path: &Path) -> Result<Config> {
        let raw = fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                "config load",
                format!("could not read {}: {e}", path.display()),
            )
        })?;
        let mut cfg: Config = toml::from_str(&raw).map_err(|e| {
            PillboxError::config(
                "config load",
                format!("{}: {e}", path.display()),
            )
        })?;
        cfg.expand_tildes();
        cfg.source = Some(path.to_path_buf());
        Ok(cfg)
    }

    fn expand_tildes(&mut self) {
        for m in &mut self.mount {
            *m = expand_tilde_in_mount(m);
        }
        for f in &mut self.env_file {
            *f = expand_tilde(f);
        }
    }
}

/// Expand a single leading `~/` against `$HOME`. Bare `~` (no slash) is
/// left alone since it's not a path. Returns the input unchanged if
/// `$HOME` is unresolvable.
fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    s.to_string()
}

/// `HOST:GUEST[:opts]` — expand `~` only in the host part.
fn expand_tilde_in_mount(spec: &str) -> String {
    match spec.split_once(':') {
        Some((host, rest)) => format!("{}:{}", expand_tilde(host), rest),
        None => expand_tilde(spec),
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
    fn discover_walks_up_to_find_config() {
        let root = TempDir::new().unwrap();
        let nested = root.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        write_config(root.path(), "name = \"frommarker\"\n");

        let cfg = Config::discover_from(&nested).unwrap().unwrap();
        assert_eq!(cfg.name.as_deref(), Some("frommarker"));
        assert_eq!(cfg.source, Some(root.path().join("pillbox.toml")));
    }

    #[test]
    fn discover_returns_none_when_no_config() {
        let root = TempDir::new().unwrap();
        let res = Config::discover_from(root.path()).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn load_parses_all_fields() {
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            r#"
name = "myapp"
env = "dev"
with = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY=OPENAI_KEY"]
mount = ["~/.aws:/home/lum/.aws:ro"]
env_file = [".env.local"]
"#,
        );
        let cfg = Config::load_from(&root.path().join("pillbox.toml")).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("myapp"));
        assert_eq!(cfg.env.as_deref(), Some("dev"));
        assert_eq!(cfg.with.len(), 2);
        assert_eq!(cfg.mount.len(), 1);
        assert!(cfg.mount[0].starts_with('/'));
        assert_eq!(cfg.env_file, vec![".env.local"]);
    }

    #[test]
    fn load_rejects_unknown_field() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "name = \"x\"\nbogus = 1\n");
        let err = Config::load_from(&root.path().join("pillbox.toml")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bogus") || msg.contains("unknown"));
    }

    #[test]
    fn tilde_expands_in_mount_host_only() {
        std::env::set_var("HOME", "/h");
        assert_eq!(
            expand_tilde_in_mount("~/.aws:/home/lum/.aws:ro"),
            "/h/.aws:/home/lum/.aws:ro"
        );
    }

    #[test]
    fn tilde_no_op_for_absolute() {
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }
}
