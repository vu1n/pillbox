//! Workspace + snapshot operations — `pillbox push`, `pillbox pull`,
//! `pillbox snapshot {list,show,rm}`, `pillbox workspace rekey`. All
//! four touch the per-pillbox rustic backend, so they share a file
//! even though each is a separate top-level command.
//!
//! Naming is verb-first to match the CLI surface (`push`, `pull`,
//! `snapshot_dispatch`, `dispatch`) — main.rs's match arms call
//! these directly.

use anyhow::Result;

use crate::cli::{SnapshotAction, WorkspaceAction};
use crate::errors::PillboxError;
use crate::paths;
use crate::pillbox::Pillbox;
use crate::workspace::{PushOptions, Snapshot, SnapshotHandle, WorkspaceBackend};

pub(crate) fn push(
    resolved: &Pillbox,
    tag: Option<String>,
    message: Option<String>,
    json: bool,
) -> Result<()> {
    let backend = resolved.workspace()?;
    let cwd = std::env::current_dir()
        .map_err(|e| PillboxError::runtime("push", format!("could not resolve cwd: {e}")))?;
    let snap = backend.push(&cwd, PushOptions { tag, message })?;
    if json {
        println!("{}", snapshot_json(&snap));
    } else {
        println!(
            "pillbox: ✓ snapshot {} ({})",
            snap.handle.short(),
            human_bytes(snap.bytes)
        );
        // `files_changed` from rustic counts files where content hash
        // moved, including newly added ones, so `files_new` is a subset
        // of `files_changed`. Surface both — "5 new, 12 changed (200
        // total)" reads more clearly than a single "changed" number.
        println!(
            "  files:      {} new, {} changed ({} total)",
            snap.files_new, snap.files_changed, snap.files_total
        );
        if let Some(t) = &snap.tag {
            println!("  tag:        {t}");
        }
        if let Some(m) = &snap.message {
            println!("  message:    {m}");
        }
        if let Some(a) = &snap.git_anchor {
            let dirty = if snap.git_dirty { " (dirty)" } else { "" };
            println!("  git anchor: {a}{dirty}");
        }
        println!("  created:    {}", snap.created_at);
    }
    Ok(())
}

pub(crate) fn pull(
    resolved: &Pillbox,
    snapshot: Option<String>,
    bookmark: Option<String>,
) -> Result<()> {
    let backend = resolved.workspace()?;
    let cwd = std::env::current_dir()
        .map_err(|e| PillboxError::runtime("pull", format!("could not resolve cwd: {e}")))?;
    let handle = match (snapshot, bookmark) {
        (Some(_), Some(_)) => {
            return Err(PillboxError::usage(
                "pull",
                "--snapshot and --bookmark are mutually exclusive",
            )
            .into());
        }
        (Some(s), None) => Some(SnapshotHandle::new(s)),
        (None, Some(name)) => Some(crate::bookmarks::resolve_existing(resolved, &name)?),
        (None, None) => None,
    };
    backend.pull(&cwd, handle.as_ref())?;
    let label = handle
        .as_ref()
        .map(|h| h.short().to_string())
        .unwrap_or_else(|| "latest".into());
    println!(
        "pillbox: ✓ restored snapshot {label} into {}",
        cwd.display()
    );
    Ok(())
}

pub(crate) fn snapshot_dispatch(resolved: &Pillbox, action: SnapshotAction) -> Result<()> {
    let backend = resolved.workspace()?;
    match action {
        SnapshotAction::List { json } => {
            let snaps = backend.snapshots()?;
            if json {
                let arr: Vec<serde_json::Value> = snaps.iter().map(snapshot_value).collect();
                println!(
                    "{}",
                    paths::json_v1(vec![
                        (
                            "pillbox",
                            serde_json::Value::String(resolved.display_name().into())
                        ),
                        ("snapshots", serde_json::Value::Array(arr)),
                    ])
                );
                return Ok(());
            }
            if snaps.is_empty() {
                println!("(no snapshots yet)");
                println!();
                println!("Run `pillbox push` to take the first snapshot.");
                return Ok(());
            }
            println!("Snapshots for `{}`:", resolved.display_name());
            for s in snaps {
                let tag = s
                    .tag
                    .as_deref()
                    .map(|t| format!(" [{t}]"))
                    .unwrap_or_default();
                println!("  {} {}{}", s.handle.short(), s.created_at, tag);
                if let Some(m) = &s.message {
                    println!("    {m}");
                }
                // git anchor — short SHA + dirty marker, mirroring
                // `git log --oneline`. Helps the user correlate a
                // snapshot back to a commit at a glance.
                if let Some(a) = &s.git_anchor {
                    let short = &a[..a.len().min(7)];
                    let dirty = if s.git_dirty { " (dirty)" } else { "" };
                    println!("    git {short}{dirty}");
                }
            }
            println!();
            println!("Use `pillbox snapshot show <HANDLE>` for details, `pillbox pull --snapshot <HANDLE>` to restore.");
        }
        SnapshotAction::Show { handle, json } => {
            let snap = backend.snapshot_show(&SnapshotHandle::new(handle))?;
            if json {
                println!("{}", snapshot_json(&snap));
            } else {
                println!("Snapshot {}", snap.handle);
                println!("  created:    {}", snap.created_at);
                if let Some(t) = &snap.tag {
                    println!("  tag:        {t}");
                }
                if let Some(m) = &snap.message {
                    println!("  message:    {m}");
                }
                if let Some(a) = &snap.git_anchor {
                    let dirty = if snap.git_dirty { " (dirty)" } else { "" };
                    println!("  git anchor: {a}{dirty}");
                }
                println!("  size:       {}", human_bytes(snap.bytes));
            }
        }
        SnapshotAction::Rm { handle } => {
            // `handle` may be a prefix the user typed; echo it back via
            // the canonical short form. Resolution already happened
            // inside `snapshot_rm`.
            let h = SnapshotHandle::new(handle.clone());
            backend.snapshot_rm(&h)?;
            println!("pillbox: ✓ removed snapshot {}", h.short());
        }
    }
    Ok(())
}

pub(crate) fn dispatch(resolved: &Pillbox, action: WorkspaceAction) -> Result<()> {
    let backend = resolved.workspace()?;
    match action {
        WorkspaceAction::Rekey => {
            backend.rekey()?;
            println!("pillbox: ✓ workspace password rotated");
            // rustic_core 0.11 exposes `add_key` but not a public
            // single-call "remove old key by password" — see the NOTE
            // in `RusticBackend::rekey`. Surface that explicitly so the
            // user isn't surprised when the previous password still
            // opens the repo. Drop this hint once rustic adds the API.
            println!();
            println!("note: rustic_core 0.11 cannot revoke the previous password from the repo;");
            println!("      treat the old password as compromised — back up + recreate the");
            println!("      pillbox if you need a hard cutover.");
        }
    }
    Ok(())
}

/// Render `bytes` as a short human-readable string (`104 B`, `4.2 KB`,
/// `1.3 MB`, …). Used by push / snapshot list / snapshot show output.
/// Binary prefixes intentionally — restic/rustic dedup math is binary
/// too, so the units line up if anyone cross-checks against the repo.
fn human_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if b < KB {
        format!("{b} B")
    } else if b < MB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else if b < GB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else {
        format!("{:.2} GB", b as f64 / GB as f64)
    }
}

fn snapshot_value(snap: &Snapshot) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert(
        "handle".into(),
        serde_json::Value::String(snap.handle.as_str().into()),
    );
    o.insert(
        "short".into(),
        serde_json::Value::String(snap.handle.short().into()),
    );
    o.insert(
        "created_at".into(),
        serde_json::Value::String(snap.created_at.clone()),
    );
    o.insert(
        "tag".into(),
        snap.tag
            .as_deref()
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    o.insert(
        "message".into(),
        snap.message
            .as_deref()
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    o.insert(
        "git_anchor".into(),
        snap.git_anchor
            .as_deref()
            .map(|s| serde_json::Value::String(s.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    o.insert("git_dirty".into(), serde_json::Value::Bool(snap.git_dirty));
    o.insert("bytes".into(), serde_json::Value::Number(snap.bytes.into()));
    o.insert(
        "files_new".into(),
        serde_json::Value::Number(snap.files_new.into()),
    );
    o.insert(
        "files_changed".into(),
        serde_json::Value::Number(snap.files_changed.into()),
    );
    o.insert(
        "files_total".into(),
        serde_json::Value::Number(snap.files_total.into()),
    );
    serde_json::Value::Object(o)
}

fn snapshot_json(snap: &Snapshot) -> String {
    paths::json_v1(vec![("snapshot", snapshot_value(snap))])
}
