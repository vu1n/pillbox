//! Snapshot bookmarks — named, movable refs to immutable workspace snapshots.
//!
//! Rustic snapshots are immutable handles. A bookmark is pillbox-owned
//! metadata that says "this name currently points at that handle". We keep
//! this separate from rustic tags because tags are attached to a snapshot at
//! creation time; bookmarks need to move without rewriting snapshot metadata.

use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::errors::PillboxError;
use crate::paths::write_private_file;
use crate::pillbox::Pillbox;
use crate::workspace::{SnapshotHandle, WorkspaceBackend};

const BOOKMARKS_FILE: &str = "bookmarks.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Bookmark {
    pub(crate) name: String,
    pub(crate) snapshot: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredBookmark {
    snapshot: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Store {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    bookmarks: BTreeMap<String, StoredBookmark>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: store_version(),
            bookmarks: BTreeMap::new(),
        }
    }
}

fn store_version() -> u32 {
    1
}

fn to_bookmark(name: &str, b: &StoredBookmark) -> Bookmark {
    Bookmark {
        name: name.to_string(),
        snapshot: b.snapshot.clone(),
        created_at: b.created_at.clone(),
        updated_at: b.updated_at.clone(),
    }
}

pub(crate) fn list(resolved: &Pillbox) -> Result<Vec<Bookmark>> {
    ensure_project(resolved, "bookmark list")?;
    let store = read_store(resolved)?;
    Ok(store
        .bookmarks
        .iter()
        .map(|(name, b)| to_bookmark(name, b))
        .collect())
}

pub(crate) fn get(resolved: &Pillbox, name: &str) -> Result<Option<Bookmark>> {
    ensure_project(resolved, "bookmark show")?;
    validate_name("bookmark show", name)?;
    let store = read_store(resolved)?;
    Ok(store.bookmarks.get(name).map(|b| to_bookmark(name, b)))
}

/// Resolve a bookmark to the canonical handle of the snapshot it points
/// at, erroring if the bookmark is unknown or its snapshot no longer
/// exists in the repo.
pub(crate) fn resolve_existing(resolved: &Pillbox, name: &str) -> Result<SnapshotHandle> {
    let bookmark = get(resolved, name)?.ok_or_else(|| {
        PillboxError::runtime("bookmark lookup", format!("bookmark `{name}` not found"))
            .with_next("pillbox bookmark list")
    })?;
    let backend = resolved.workspace()?;
    let snap = backend.snapshot_show(&SnapshotHandle::new(bookmark.snapshot))?;
    Ok(snap.handle)
}

pub(crate) fn set(resolved: &Pillbox, name: &str, snapshot_spec: Option<&str>) -> Result<Bookmark> {
    ensure_project(resolved, "bookmark set")?;
    validate_name("bookmark set", name)?;
    let backend = resolved.workspace()?;
    let snap = resolve_snapshot_spec(&backend, snapshot_spec)?;
    let now = crate::session::now_rfc3339();
    let mut store = read_store(resolved)?;
    let created_at = store
        .bookmarks
        .get(name)
        .map(|b| b.created_at.clone())
        .unwrap_or_else(|| now.clone());
    let record = StoredBookmark {
        snapshot: snap.handle.as_str().to_string(),
        created_at: created_at.clone(),
        updated_at: now.clone(),
    };
    store.bookmarks.insert(name.to_string(), record);
    write_store(resolved, &store)?;
    Ok(Bookmark {
        name: name.to_string(),
        snapshot: snap.handle.as_str().to_string(),
        created_at,
        updated_at: now,
    })
}

pub(crate) fn delete(resolved: &Pillbox, name: &str) -> Result<bool> {
    ensure_project(resolved, "bookmark rm")?;
    validate_name("bookmark rm", name)?;
    let mut store = read_store(resolved)?;
    let removed = store.bookmarks.remove(name).is_some();
    write_store(resolved, &store)?;
    Ok(removed)
}

pub(crate) fn resolve_snapshot_spec(
    backend: &impl WorkspaceBackend,
    snapshot_spec: Option<&str>,
) -> Result<crate::workspace::Snapshot> {
    match snapshot_spec {
        None | Some("latest") => {
            let mut snaps = backend.snapshots()?;
            snaps.pop().ok_or_else(|| {
                PillboxError::runtime(
                    "bookmark set",
                    "no snapshots exist yet; run `pillbox push` first",
                )
                .into()
            })
        }
        Some(spec) => backend.snapshot_show(&SnapshotHandle::new(spec.to_string())),
    }
}

pub(crate) fn validate_name(action: &'static str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(PillboxError::usage(action, "bookmark name cannot be empty").into());
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err(PillboxError::usage(
            action,
            format!("bookmark name `{name}` cannot start/end with `/` or contain `//`"),
        )
        .into());
    }
    for part in name.split('/') {
        if part == "." || part == ".." {
            return Err(PillboxError::usage(
                action,
                format!("bookmark name `{name}` cannot contain `.` or `..` path segments"),
            )
            .into());
        }
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
    {
        return Err(PillboxError::usage(
            action,
            format!("bookmark name `{name}` must be ASCII alphanumeric plus `_`, `-`, `.`, or `/`"),
        )
        .into());
    }
    Ok(())
}

fn ensure_project(resolved: &Pillbox, action: &'static str) -> Result<()> {
    if resolved.meta.is_none() {
        return Err(PillboxError::usage(
            action,
            "the global pillbox has no workspace bookmarks; cd into a project pillbox",
        )
        .into());
    }
    Ok(())
}

fn store_path(resolved: &Pillbox) -> PathBuf {
    resolved.state_dir.join(BOOKMARKS_FILE)
}

fn read_store(resolved: &Pillbox) -> Result<Store> {
    let path = store_path(resolved);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let store: Store = serde_json::from_str(&raw)
        .map_err(|e| PillboxError::config("bookmark read", format!("{}: {e}", path.display())))?;
    if store.version != 1 {
        return Err(PillboxError::config(
            "bookmark read",
            format!(
                "{}: bookmark store version {} is not supported",
                path.display(),
                store.version
            ),
        )
        .into());
    }
    Ok(store)
}

fn write_store(resolved: &Pillbox, store: &Store) -> Result<()> {
    // `store` always carries version 1 — `read_store` rejects anything
    // else and `Store::default` starts at 1 — so serialize it as-is.
    let body = serde_json::to_vec_pretty(store)
        .map_err(|e| PillboxError::runtime("bookmark write", format!("serialize: {e}")))?;
    write_private_file(&store_path(resolved), &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmark_names_accept_slash_refs() {
        validate_name("bookmark set", "session/abc-123").unwrap();
        validate_name("bookmark set", "stable.v1").unwrap();
    }

    #[test]
    fn bookmark_names_reject_path_traversal_shapes() {
        for bad in ["", "/main", "main/", "session//x", "../x", "x/../y", "x y"] {
            assert!(validate_name("bookmark set", bad).is_err(), "{bad}");
        }
    }
}
