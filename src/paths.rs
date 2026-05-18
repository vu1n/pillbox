//! Shared `~/.pillbox/` directory helpers. Every creator goes through
//! here so the parent's perms get pinned to 0700 on every touch
//! (`fs::create_dir_all` uses the process umask, typically 022 → 755).

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
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

pub(crate) fn data_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("could not resolve $HOME")?;
    let root = PathBuf::from(home).join(".pillbox");
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {} 0700", root.display()))?;
    Ok(root)
}

pub(crate) fn data_subdir(name: &str) -> Result<PathBuf> {
    let dir = data_root()?.join(name);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {} 0700", dir.display()))?;
    Ok(dir)
}
