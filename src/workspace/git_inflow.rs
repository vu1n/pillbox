//! Git-as-inflow helper.
//!
//! Pillbox uses git only as an **inflow at workspace creation time**:
//! `pillbox new --from-git URL` clones a repo into cwd so the
//! subsequent rustic snapshots have something to snapshot. Once the
//! workspace exists, git is not used as a storage backend — every
//! versioning op goes through rustic. We do, however, peek at git's
//! HEAD on `pillbox push` so the snapshot record carries a `git_anchor`
//! SHA that humans can correlate to a commit.
//!
//! Implementation: `git` as a subprocess. The git2 crate would pull in
//! libgit2 + ~200K LOC of C; we need exactly two operations (clone +
//! `rev-parse HEAD`/`status --porcelain`) and the user already has git
//! installed (we're a developer tool). Subprocess wins on size and on
//! "no unexpected library state".

use std::{path::Path, process::Command};

use anyhow::Result;

use crate::errors::PillboxError;

/// Clone `url` into `dest`. Optionally checkout a ref (branch or SHA).
/// Returns the resolved HEAD SHA on success.
///
/// `dest` must be empty (git clone refuses an existing non-empty target
/// dir, which is what we want — `pillbox new --from-git` runs at
/// pillbox-creation time, before any other writes).
pub(crate) fn clone_into(url: &str, dest: &Path, git_ref: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("clone");
    if let Some(r) = git_ref {
        cmd.arg("--branch").arg(r);
    }
    cmd.arg(url).arg(dest);
    let out = cmd.output().map_err(|e| {
        PillboxError::resource(
            "workspace clone",
            format!("could not invoke git: {e} (is git installed?)"),
        )
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(PillboxError::runtime(
            "workspace clone",
            format!("git clone {url} failed: {}", stderr.trim()),
        )
        .into());
    }
    // If `git_ref` was a SHA (not a branch), `--branch` would have
    // failed above; rerun checkout for that case. Best-effort — the
    // tree may have been left at default HEAD otherwise.
    if let Some(r) = git_ref {
        let chk = Command::new("git")
            .arg("-C")
            .arg(dest)
            .args(["checkout", r])
            .output();
        if let Ok(o) = chk {
            if !o.status.success() {
                // Non-fatal: branch checkout already worked.
            }
        }
    }
    let sha = resolve_head(dest)?.unwrap_or_default();
    Ok(sha)
}

/// Read git HEAD + working-tree dirty bit if `cwd` is inside a git
/// working tree. Returns `Some((sha, dirty))` on success, `None` if
/// `cwd` isn't a git working tree. Errors only on a real failure
/// invoking git — the "no .git here" path is the most common and is
/// just `None`.
pub(crate) fn resolve_git_anchor(cwd: &Path) -> Result<(Option<String>, bool)> {
    if !is_git_worktree(cwd) {
        return Ok((None, false));
    }
    let head = resolve_head(cwd)?;
    let dirty = is_dirty(cwd)?;
    Ok((head, dirty))
}

fn is_git_worktree(cwd: &Path) -> bool {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    match out {
        Ok(o) => o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true",
        Err(_) => false,
    }
}

fn resolve_head(cwd: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| {
            PillboxError::resource("git rev-parse", format!("could not invoke git: {e}"))
        })?;
    if !out.status.success() {
        // Common case: empty repo, no commits yet. Not an error.
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha))
    }
}

fn is_dirty(cwd: &Path) -> Result<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| PillboxError::resource("git status", format!("could not invoke git: {e}")))?;
    if !out.status.success() {
        return Ok(false);
    }
    Ok(!out.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as PCommand;

    /// Build a tiny throwaway git repo + one commit. Returns the repo
    /// path. Skips the test if `git` isn't installed (CI without git
    /// shouldn't fail — every other test would already be broken).
    fn make_repo() -> Option<tempfile::TempDir> {
        if PCommand::new("git").arg("--version").output().is_err() {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            PCommand::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example"]);
        run(&["config", "user.name", "t"]);
        // `commit.gpgsign` defaults can break this on dev machines.
        run(&["config", "commit.gpgsign", "false"]);
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        Some(dir)
    }

    #[test]
    fn resolve_git_anchor_on_clean_repo() {
        let Some(d) = make_repo() else { return };
        let (sha, dirty) = resolve_git_anchor(d.path()).unwrap();
        assert!(sha.is_some(), "expected a HEAD SHA");
        assert!(!dirty, "fresh commit should be clean");
        let s = sha.unwrap();
        assert_eq!(s.len(), 40, "got: {s}");
    }

    #[test]
    fn resolve_git_anchor_on_dirty_repo() {
        let Some(d) = make_repo() else { return };
        // Modify a tracked file → dirty.
        fs::write(d.path().join("a.txt"), b"changed").unwrap();
        let (sha, dirty) = resolve_git_anchor(d.path()).unwrap();
        assert!(sha.is_some());
        assert!(dirty);
    }

    #[test]
    fn resolve_git_anchor_on_non_git_dir() {
        let d = tempfile::tempdir().unwrap();
        let (sha, dirty) = resolve_git_anchor(d.path()).unwrap();
        assert!(sha.is_none());
        assert!(!dirty);
    }

    #[test]
    fn clone_into_local_bare_repo() {
        // Build a local bare repo with one commit; clone it.
        let Some(src) = make_repo() else { return };
        let bare = tempfile::tempdir().unwrap();
        let out = PCommand::new("git")
            .args(["clone", "--bare"])
            .arg(src.path())
            .arg(bare.path().join("repo.git"))
            .output()
            .unwrap();
        assert!(out.status.success(), "bare clone setup failed");

        let dest = tempfile::tempdir().unwrap();
        let dest_path = dest.path().join("checkout");
        let url = format!("file://{}", bare.path().join("repo.git").display());
        let sha = clone_into(&url, &dest_path, None).unwrap();
        assert!(dest_path.join("a.txt").exists());
        assert_eq!(sha.len(), 40, "expected full SHA, got: {sha}");
    }
}
