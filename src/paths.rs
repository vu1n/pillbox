//! Shared `~/.pillbox/` directory helpers.
//!
//! v0.6 layout — pillbox is now a bundle, not a flat namespace:
//!
//! ```text
//! ~/.pillbox/                   (0700)
//! ├── global/                   (0700)         — global pillbox
//! │   ├── secrets/
//! │   ├── env/
//! │   ├── auth/{claude,codex}/  — agent OAuth state, shared across projects
//! │   └── vault/                — CA + key
//! └── projects/                 (0700)
//!     └── -Users-vuln-code-foo/ — one per dir with pillbox.toml
//!         ├── meta.json
//!         ├── secrets/          — overrides global on key conflict
//!         ├── env/
//!         ├── auth/             — reserved (per-project auth deferred to v0.7)
//!         └── vault/
//! ```
//!
//! Every creator goes through here so the parent's perms get pinned to
//! 0700 on every touch (`fs::create_dir_all` uses the process umask,
//! typically 022 → 755).

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::errors::PillboxError;

/// Shared filename rule for secrets, env bundles, and any other named
/// per-file artifacts. Blocks `../` traversal and keeps names portable.
pub(crate) fn validate_name(action: &'static str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(PillboxError::usage(action, "name cannot be empty").into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(PillboxError::usage(
            action,
            format!("name `{name}` must be ASCII alphanumeric plus `_`, `-`, or `.`"),
        )
        .into());
    }
    Ok(())
}

/// `~/.pillbox/` itself — 0700, created lazily. Every per-scope dir
/// (`global/`, `projects/<key>/`) sits under this.
pub(crate) fn pillbox_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("could not resolve $HOME")?;
    let root = PathBuf::from(home).join(".pillbox");
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    ensure_mode_0700(&root)?;
    Ok(root)
}

/// Idempotently apply 0700 perms to a directory we created or expect to
/// own. Called on every touch so a stray `chmod -R 755` somewhere else
/// gets tightened back on the next pillbox invocation.
pub(crate) fn ensure_mode_0700(p: &Path) -> Result<()> {
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {} 0700", p.display()))?;
    Ok(())
}

/// Serialize a pillbox JSON output with the standard `version: 1`
/// envelope. Every `--json` payload goes through this so consumers can
/// pin against the version field. Bump the constant on a breaking change.
pub(crate) fn json_v1(fields: Vec<(&'static str, serde_json::Value)>) -> String {
    let mut root = serde_json::Map::new();
    root.insert("version".into(), serde_json::Value::Number(1.into()));
    for (k, v) in fields {
        root.insert(k.into(), v);
    }
    serde_json::Value::Object(root).to_string()
}

/// Subdirectory names that, if present at the top level of `~/.pillbox/`,
/// indicate a v0.5 install. v0.6 keeps the same names but only under
/// `global/` and `projects/<key>/`, so a top-level hit is a clean signal
/// without false positives. Used by `pillbox` (init/lifecycle guards) and
/// `doctor` (warning surface). Single source so the two stay in sync.
pub(crate) const V0_5_LEGACY_SUBDIRS: &[&str] = &["data", "secrets", "env", "vault"];

/// Returns the legacy v0.5 subdir names that exist directly under `root`.
/// `root` is expected to be `~/.pillbox/`. Empty vec means no legacy
/// layout detected.
pub(crate) fn detect_legacy_subdirs(root: &Path) -> Vec<&'static str> {
    V0_5_LEGACY_SUBDIRS
        .iter()
        .copied()
        .filter(|name| root.join(name).is_dir())
        .collect()
}

/// Process-wide lock for tests that mutate `$HOME`. Shared so tests
/// across `secrets` / `agents` / `vault` modules can't race each other
/// when cargo runs them on multiple threads.
#[cfg(test)]
pub(crate) static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_normal_names() {
        assert!(validate_name("secret add", "ANTHROPIC_API_KEY").is_ok());
        assert!(validate_name("env load", "db.staging").is_ok());
        assert!(validate_name("secret add", "foo-bar").is_ok());
    }

    #[test]
    fn validate_name_rejects_escape_attempts() {
        assert!(validate_name("secret add", "../etc/passwd").is_err());
        assert!(validate_name("secret add", "foo/bar").is_err());
        assert!(validate_name("secret add", "").is_err());
        assert!(validate_name("secret add", "foo bar").is_err());
    }
}
