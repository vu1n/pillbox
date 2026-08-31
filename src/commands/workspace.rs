//! Workspace + snapshot operations — `pillbox push`, `pillbox pull`,
//! `pillbox snapshot {list,show,rm}`, `pillbox workspace rekey`. All
//! four touch the per-pillbox rustic backend, so they share a file
//! even though each is a separate top-level command.
//!
//! Naming is verb-first to match the CLI surface (`push`, `pull`,
//! `snapshot_dispatch`, `dispatch`) — main.rs's match arms call
//! these directly.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::cli::{
    RemoteRepoBackup, RemoteRepoCoords, RemoteRepoRestore, SnapshotAction, WorkspaceAction,
};
use crate::errors::PillboxError;
use crate::paths;
use crate::pillbox::Pillbox;
use crate::workspace::rustic::{RusticBackend, RusticVariant, S3Config};
use crate::workspace::{PushOptions, Snapshot, SnapshotHandle, WorkspaceBackend};

pub(crate) fn push(
    resolved: &Pillbox,
    tag: Option<String>,
    message: Option<String>,
    bookmark: Option<String>,
    parents: Vec<String>,
    json: bool,
) -> Result<()> {
    let backend = resolved.workspace()?;
    let cwd = std::env::current_dir()
        .map_err(|e| PillboxError::runtime("push", format!("could not resolve cwd: {e}")))?;
    let snap = backend.push(
        &cwd,
        PushOptions {
            tag,
            message,
            parents,
        },
    )?;
    // Bind the bookmark to THIS snapshot by handle (not `latest`) so a concurrent
    // push can't shift it. The snapshot already exists, so a bookmark failure
    // (e.g. global pillbox — bookmarks need a project) doesn't lose the snapshot.
    if let Some(name) = bookmark.as_deref() {
        crate::bookmarks::set(resolved, name, Some(snap.handle.as_str()))?;
    }
    if json {
        println!(
            "{}",
            snapshot_json_with_bookmark(&snap, bookmark.as_deref())
        );
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
        if !snap.parents.is_empty() {
            let shorts: Vec<&str> = snap.parents.iter().map(|p| &p[..p.len().min(8)]).collect();
            println!("  parents:    {}", shorts.join(", "));
        }
        println!("  created:    {}", snap.created_at);
        if let Some(name) = bookmark.as_deref() {
            println!("  bookmark:   {name}");
        }
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
        // Routed in main.rs BEFORE pillbox resolution (they're
        // self-contained from flags+env). Unreachable here, but matched
        // explicitly so adding a variant can't silently fall through.
        WorkspaceAction::Restore(args) => return remote_restore(args),
        WorkspaceAction::Backup(args) => return remote_backup(args),
    }
    Ok(())
}

// ── managed-tier standalone repo ops ────────────────────────────────────────
//
// `workspace restore` / `workspace backup` operate on a rustic-on-S3 repo
// from explicit coordinates, with NO pillbox / meta.json / state dir. The
// managed-tier Durable Object execs these inside a bare container to pull
// the workspace in before the agent runs and push results out after. They
// reuse the existing `RusticBackend` S3 paths (the same `push`/`pull` the
// per-pillbox commands above call) — the only new surface is building the
// backend from flags+env instead of a resolved pillbox.

/// Restore `--snapshot` into `--target`, addressing the repo by explicit
/// S3 coordinates + env-only secrets. Reuses `RusticBackend::pull`.
pub(crate) fn remote_restore(args: RemoteRepoRestore) -> Result<()> {
    let target = PathBuf::from(&args.target);
    // Create the target up front: `pull` restores into an existing dir
    // (mirrors `pillbox pull` over cwd), and the managed container hands
    // us a fresh path that may not exist yet.
    std::fs::create_dir_all(&target).map_err(|e| {
        PillboxError::runtime(
            "workspace restore",
            format!("create target dir {}: {e}", target.display()),
        )
    })?;

    let pw = RepoPassword::from_env("workspace restore")?;
    let backend = remote_backend("workspace restore", &args.coords, pw.path())?;
    let handle = SnapshotHandle::new(args.snapshot);
    backend.pull(&target, Some(&handle))?;
    println!(
        "pillbox: ✓ restored snapshot {} into {}",
        handle.short(),
        target.display()
    );
    Ok(())
}

/// Snapshot `--target` into the repo addressed by explicit S3 coordinates
/// with env-only secrets. Reuses `RusticBackend::push` and prints ONLY
/// the new snapshot handle as the final stdout line (the DO captures it
/// as the result handle).
pub(crate) fn remote_backup(args: RemoteRepoBackup) -> Result<()> {
    let target = PathBuf::from(&args.target);
    if !target.is_dir() {
        return Err(PillboxError::usage(
            "workspace backup",
            format!("target {} is not a directory", target.display()),
        )
        .into());
    }

    let pw = RepoPassword::from_env("workspace backup")?;
    let backend = remote_backend("workspace backup", &args.coords, pw.path())?;
    let snap = backend.push(
        &target,
        PushOptions {
            parents: vec![args.parent],
            ..PushOptions::default()
        },
    )?;
    // The full 64-hex handle is the contract output — the DO reads it as
    // the final stdout line. Keep it the LAST thing printed; status goes
    // to stderr so stdout stays a clean single-line handle.
    eprintln!(
        "pillbox: ✓ snapshot {} ({})",
        snap.handle.short(),
        human_bytes(snap.bytes)
    );
    println!("{}", snap.handle.as_str());
    Ok(())
}

/// Build an S3-backed `RusticBackend` from non-secret coordinates +
/// env-resolved credentials. `password_file` points at a temp 0600 file
/// holding `PILLBOX_REPO_PASSWORD` (see [`RepoPassword`]).
fn remote_backend(
    action: &'static str,
    coords: &RemoteRepoCoords,
    password_file: &Path,
) -> Result<RusticBackend> {
    let access_key = require_env(action, "PILLBOX_R2_ACCESS_KEY")?;
    let secret_key = require_env(action, "PILLBOX_R2_SECRET_KEY")?;
    // R2 temp credentials are an STS-style triple: access key, secret key,
    // session token. Long-lived credentials omit the token, so keep it optional
    // while ensuring a scoped credential reaches rustic intact.
    let session_token = std::env::var("PILLBOX_R2_SESSION_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    Ok(RusticBackend {
        variant: RusticVariant::S3(S3Config {
            endpoint: coords.endpoint.clone(),
            region: coords.region.clone(),
            bucket: coords.bucket.clone(),
            prefix: coords.prefix.clone(),
            access_key,
            secret_key,
            session_token,
        }),
        password_file: password_file.to_path_buf(),
    })
}

/// The repo encryption password sourced from `PILLBOX_REPO_PASSWORD`,
/// materialized as a temp 0600 file because `RusticBackend` reads its
/// password from disk (`read_password`). The temp dir auto-removes on
/// drop, so the password never lingers after the command returns — and it
/// never touches argv, where another process could read it.
struct RepoPassword {
    // Held only for its Drop (RAII cleanup of the temp dir + file).
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl RepoPassword {
    fn from_env(action: &'static str) -> Result<Self> {
        let password = require_env(action, "PILLBOX_REPO_PASSWORD")?;
        let dir = tempfile::Builder::new()
            .prefix("pillbox-repo-pw")
            .tempdir()
            .map_err(|e| {
                PillboxError::runtime(action, format!("create temp dir for repo password: {e}"))
            })?;
        let path = dir.path().join("repo-password");
        // 0600 via the shared private-file writer so the on-disk perms
        // invariant stays in one place (matches the per-pillbox password).
        crate::paths::write_private_file(&path, password.as_bytes())?;
        Ok(Self { _dir: dir, path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Read a REQUIRED secret env var, erroring (exit 3 / config) by name if
/// it's unset or empty. Secrets come from the environment ONLY — never
/// flags/argv — so a missing one is a configuration error, not usage.
fn require_env(action: &'static str, var: &'static str) -> Result<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(PillboxError::config(
            action,
            format!("missing required environment variable {var}"),
        )
        .with_next(format!(
            "set {var} in the environment (never pass secrets as flags)"
        ))
        .into()),
    }
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
    o.insert(
        "parents".into(),
        serde_json::Value::Array(
            snap.parents
                .iter()
                .map(|p| serde_json::Value::String(p.clone()))
                .collect(),
        ),
    );
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

/// `push --json`, including the bookmark it set (null when `--bookmark` absent).
fn snapshot_json_with_bookmark(snap: &Snapshot, bookmark: Option<&str>) -> String {
    let bm = bookmark
        .map(|s| serde_json::Value::String(s.to_string()))
        .unwrap_or(serde_json::Value::Null);
    paths::json_v1(vec![("snapshot", snapshot_value(snap)), ("bookmark", bm)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ExitCategory;

    // `require_env` reads PROCESS-global env; serialize the env-mutating
    // tests so they don't race each other (cargo runs tests in parallel).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn err_is_config_missing(action: &'static str, var: &'static str) {
        let err = require_env(action, var).unwrap_err();
        let pb = err
            .downcast_ref::<PillboxError>()
            .expect("missing-env must be a PillboxError");
        // Config error → exit 3 (per the contract: missing secret env var
        // is a configuration problem, not a usage one).
        assert_eq!(pb.category as u8, ExitCategory::Config as u8);
        // The message must NAME the missing var so the DO/operator knows
        // which one to set.
        assert!(
            pb.reason.contains(var),
            "reason must name {var}, got: {}",
            pb.reason
        );
    }

    #[test]
    fn require_env_errors_config_for_each_missing_secret() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for var in [
            "PILLBOX_R2_ACCESS_KEY",
            "PILLBOX_R2_SECRET_KEY",
            "PILLBOX_REPO_PASSWORD",
        ] {
            std::env::remove_var(var);
            err_is_config_missing("workspace restore", var);
        }
    }

    #[test]
    fn require_env_treats_empty_value_as_missing() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // An empty string is as unusable as unset — guard rejects both so
        // a blank secret can't silently produce a broken repo handle.
        std::env::set_var("__PB_TEST_EMPTY_SECRET", "");
        let err = require_env("workspace backup", leak_static("__PB_TEST_EMPTY_SECRET"));
        let err = err.unwrap_err();
        let pb = err.downcast_ref::<PillboxError>().unwrap();
        assert_eq!(pb.category as u8, ExitCategory::Config as u8);
        std::env::remove_var("__PB_TEST_EMPTY_SECRET");
    }

    #[test]
    fn require_env_returns_value_when_set() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("__PB_TEST_PRESENT_SECRET", "v");
        let got = require_env("workspace backup", leak_static("__PB_TEST_PRESENT_SECRET")).unwrap();
        assert_eq!(got, "v");
        std::env::remove_var("__PB_TEST_PRESENT_SECRET");
    }

    #[test]
    fn remote_backend_forwards_optional_r2_session_token() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("PILLBOX_R2_ACCESS_KEY", "scoped-ak");
        std::env::set_var("PILLBOX_R2_SECRET_KEY", "scoped-sk");
        std::env::set_var("PILLBOX_R2_SESSION_TOKEN", "scoped-session-token");

        let coords = RemoteRepoCoords {
            endpoint: "https://account.r2.cloudflarestorage.com".into(),
            bucket: "workspaces".into(),
            region: "auto".into(),
            prefix: "project/run/".into(),
        };
        let backend = remote_backend("workspace restore", &coords, Path::new("/tmp/password"))
            .expect("scoped R2 credentials should build a backend");
        let RusticVariant::S3(config) = backend.variant else {
            panic!("remote workspace backend must use S3");
        };
        assert_eq!(
            config.session_token.as_deref(),
            Some("scoped-session-token")
        );

        std::env::remove_var("PILLBOX_R2_ACCESS_KEY");
        std::env::remove_var("PILLBOX_R2_SECRET_KEY");
        std::env::remove_var("PILLBOX_R2_SESSION_TOKEN");
    }

    #[test]
    fn repo_password_writes_temp_0600_file_and_cleans_up() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("PILLBOX_REPO_PASSWORD", "topsecret-pw");
        let pw = RepoPassword::from_env("workspace restore").unwrap();
        let path = pw.path().to_path_buf();
        // Materialized 0600 (matches the per-pillbox password invariant).
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "temp password file must be 0600");
        // Contents are the env value verbatim (RusticBackend trims on read).
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "topsecret-pw");
        // Drop removes the temp dir + file so the secret doesn't linger.
        drop(pw);
        assert!(!path.exists(), "temp password file must be removed on drop");
        std::env::remove_var("PILLBOX_REPO_PASSWORD");
    }

    // `require_env` takes `&'static str`; the test-only var names above are
    // string literals so this leak is bounded to the (tiny) test set.
    fn leak_static(s: &str) -> &'static str {
        Box::leak(s.to_string().into_boxed_str())
    }
}
