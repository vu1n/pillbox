//! `pillbox collect` — batch-retrieve finished session results + lineage.
//!
//! The substrate half of an orchestrator's fan-out / merge loop: an
//! orchestrator forks work into independent pillbox sessions, then `collect`s
//! every finished result tree into `<to>/<session>/` and reports the **merge
//! triple handles** (`base_snapshot`/`base_git_anchor` = the fork point and the
//! merge base commit; `result_snapshot` = *theirs*). It deliberately stops at
//! the merge decision — pillbox owns the mechanism, the orchestrator owns the
//! policy (select-one / union / three-way merge / hand conflicts to an agent).
//!
//! `collect` is the layer `dispatch` is a special case of: `dispatch` =
//! `collect` + grade + select-one. See [docs/collect.md](../../docs/collect.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::commands::session::{live_workspace_clone, rehydrate_result, RehydrateSource};
use crate::errors::PillboxError;
use crate::paths;
use crate::pillbox::Pillbox;
use crate::session;
use crate::workspace::WorkspaceBackend;

/// One collected session in the `--json` manifest. Carries the lineage handles
/// an orchestrator needs to merge however it decides — `base_git_anchor` is the
/// merge base commit; `dir` is where *theirs* was rehydrated.
struct CollectEntry {
    session: String,
    base_snapshot: Option<String>,
    base_git_anchor: Option<String>,
    result_snapshot: Option<String>,
    result_git_anchor: Option<String>,
    dir: PathBuf,
    /// `"snapshot"` (canonical `result_snapshot`) or `"live_clone"` (libkrun
    /// headless session recovered from its live workspace clone).
    source: &'static str,
    /// `refs/pillbox/collect/<session>` when `--as-refs` synthesized a git
    /// commit for this result; `None` otherwise (serialized as the `ref` key).
    git_ref: Option<String>,
}

impl CollectEntry {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "session": self.session,
            "base_snapshot": self.base_snapshot,
            "base_git_anchor": self.base_git_anchor,
            "result_snapshot": self.result_snapshot,
            "result_git_anchor": self.result_git_anchor,
            "dir": self.dir.display().to_string(),
            "source": self.source,
            "ref": self.git_ref,
        })
    }
}

/// `pillbox collect SESSION… [--to DIR] [--as-refs] [--json]`.
pub(crate) fn collect(
    resolved: &Pillbox,
    sessions: Vec<String>,
    to: Option<PathBuf>,
    as_refs: bool,
    json: bool,
) -> Result<()> {
    // Resolve every id up front (prefix-friendly), so a typo fails before we
    // rehydrate anything.
    let resolved_sessions: Vec<session::Session> = sessions
        .iter()
        .map(|id| session::resolve(resolved, id))
        .collect::<Result<_>>()?;

    // The resolved id keys a filesystem path below (`to_dir.join(&s.id)`). Ids
    // are minted as 12 hex chars; reject any other shape (a corrupted/hand-
    // edited registry record) before it can escape --to via `../`. The state
    // dir is uid-owned 0700, so this is defense-in-depth, not a trust boundary;
    // the deeper fix is a registry-parse invariant (noted for ship-review).
    for s in &resolved_sessions {
        if s.id.len() != 12 || !s.id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(PillboxError::runtime(
                "collect",
                format!("session id `{}` is malformed (expected 12 hex chars)", s.id),
            )
            .with_next("pillbox session list")
            .into());
        }
    }

    // Validate up front: a session with no result yet (agent unfinished / torn
    // down) fails the batch BEFORE any rehydrate, with a clear list, rather than
    // writing a silent partial set. This is the no-result guard, not a
    // transactional rollback — a rare mid-loop I/O failure can leave already-
    // collected dirs, so an orchestrator treats a non-zero exit as untrusted
    // output. One wanting "collect whatever's ready" filters the session list
    // itself (it knows what finished via wait-idle / score).
    let pending: Vec<&str> = resolved_sessions
        .iter()
        .filter(|s| s.result_snapshot.is_none() && live_workspace_clone(s).is_none())
        .map(|s| s.id.as_str())
        .collect();
    if let Some(first) = pending.first() {
        return Err(PillboxError::runtime(
            "collect",
            format!(
                "no result yet for {} session(s): {} — the agent hasn't \
                 finished or the session was torn down",
                pending.len(),
                pending.join(", ")
            ),
        )
        .with_next(format!("pillbox session wait-idle {first}"))
        .into());
    }

    // Absolutize the target so the manifest's `to`/`dir` are cwd-independent —
    // an orchestrator resolves them from a different cwd. Default lands under
    // ./collected; an explicit relative --to is joined onto cwd too.
    let cwd = std::env::current_dir()
        .map_err(|e| PillboxError::runtime("collect", format!("resolve cwd: {e}")))?;
    let to_dir = match to {
        Some(p) if p.is_absolute() => p,
        Some(p) => cwd.join(p),
        None => cwd.join("collected"),
    };

    // --as-refs synthesizes git commits in the ORIGINATING repo (cwd), so
    // require it to be a git work tree up front — before any rehydrate — rather
    // than failing mid-batch after writing trees to disk.
    let repo_root = if as_refs {
        Some(require_git_worktree(&cwd)?)
    } else {
        None
    };

    // Build handle → git_anchor once (a single repo open) so we don't pay
    // rustic's scrypt per snapshot. Best-effort: a repo-less pillbox (global,
    // live-clone only) yields an empty map and null anchors — fine, anchors are
    // correlation metadata, not load-bearing for rehydration.
    let anchors = git_anchor_map(resolved);

    // rehydrate_result opens the rustic repo once per snapshot-sourced session
    // (rustic has no batch restore). Acceptable — collect runs over a small
    // fan-out, not a hot path; a single-open batch restore is deferred backend
    // work, not an oversight.
    let mut entries = Vec::with_capacity(resolved_sessions.len());
    for s in &resolved_sessions {
        let dir = to_dir.join(&s.id);
        let source = match rehydrate_result(resolved, s, &dir, "collect")? {
            RehydrateSource::Snapshot(_) => "snapshot",
            RehydrateSource::LiveClone => "live_clone",
        };
        let base_git_anchor = anchor_of(&anchors, s.base_snapshot.as_deref());
        let git_ref = match &repo_root {
            Some(root) => Some(write_result_ref(
                root,
                &s.id,
                &dir,
                base_git_anchor.as_deref(),
            )?),
            None => None,
        };
        entries.push(CollectEntry {
            session: s.id.clone(),
            result_git_anchor: anchor_of(&anchors, s.result_snapshot.as_deref()),
            base_snapshot: s.base_snapshot.clone(),
            result_snapshot: s.result_snapshot.clone(),
            base_git_anchor,
            dir,
            source,
            git_ref,
        });
    }

    if json {
        let arr: Vec<serde_json::Value> = entries.iter().map(CollectEntry::to_json).collect();
        println!(
            "{}",
            paths::json_v1(vec![
                (
                    "pillbox",
                    serde_json::Value::String(resolved.display_name().into()),
                ),
                (
                    "to",
                    serde_json::Value::String(to_dir.display().to_string())
                ),
                ("results", serde_json::Value::Array(arr)),
            ])
        );
    } else {
        println!(
            "pillbox: ✓ collected {} session(s) into {}",
            entries.len(),
            to_dir.display()
        );
        for e in &entries {
            let base = e
                .base_git_anchor
                .as_deref()
                .map(|a| &a[..a.len().min(12)])
                .unwrap_or("(none)");
            println!("  {} → {} (base {})", e.session, e.dir.display(), base);
            if let Some(r) = &e.git_ref {
                println!("      ref: {r}");
            }
        }
    }
    Ok(())
}

/// Map every snapshot handle in the pillbox's repo to its git anchor, in one
/// repo open. Best-effort — a repo-less or unreadable backend yields an empty
/// map (anchors then surface as `null`, which the contract allows).
fn git_anchor_map(resolved: &Pillbox) -> HashMap<String, String> {
    resolved
        .workspace()
        .and_then(|b| b.snapshots())
        .map(|snaps| {
            snaps
                .into_iter()
                .filter_map(|s| s.git_anchor.map(|a| (s.handle.as_str().to_string(), a)))
                .collect()
        })
        .unwrap_or_default()
}

fn anchor_of(anchors: &HashMap<String, String>, handle: Option<&str>) -> Option<String> {
    handle.and_then(|h| anchors.get(h).cloned())
}

// ── --as-refs: git-commit synthesis ─────────────────────────────────────────
//
// Project each rehydrated result tree into the ORIGINATING repo as a commit
// rooted at its merge base, written under refs/pillbox/collect/<session>. git is
// a subprocess (matching workspace::git_inflow — the user already has git, and
// we avoid a libgit2 dependency). pillbox never merges; this just hands the
// orchestrator merge-ready refs (`git merge <ref>` / diff base→ref).

/// Resolve the git work-tree root containing `cwd`, or a clear error if `cwd`
/// isn't inside one (`--as-refs` needs a repo to write refs into).
fn require_git_worktree(cwd: &Path) -> Result<PathBuf> {
    git_out(cwd, None, None, false, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|_| {
            PillboxError::usage(
                "collect",
                "--as-refs needs the current directory to be a git work tree \
                 (it writes refs/pillbox/collect/<session> into it)",
            )
            .with_next("git init  # or re-run collect without --as-refs")
            .into()
        })
}

/// Synthesize a git commit for a rehydrated result tree under
/// `refs/pillbox/collect/<session>` in `repo_root`, returning the ref name. The
/// commit's tree is `result_dir`; its parent is `base` when that commit exists
/// in the repo — so an orchestrator's `git merge <ref>` (or diff base→ref) is
/// exactly the worker's changes. `.gitignore` is respected, so the ref is a
/// mergeable code tree; `--to DIR` still holds the full workspace.
fn write_result_ref(
    repo_root: &Path,
    session: &str,
    result_dir: &Path,
    base: Option<&str>,
) -> Result<String> {
    // A throwaway index keeps the repo's real index/working tree untouched.
    // Keyed by pid + the (validated 12-hex) session id so two concurrent
    // collects of the same session (e.g. a retry overlapping its predecessor)
    // can't clobber each other's index; cleared first in case a prior run
    // crashed mid-build.
    let index = std::env::temp_dir().join(format!(
        "pillbox-collect-{}-{session}.index",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&index);
    let tree = build_result_tree(repo_root, result_dir, &index);
    let _ = std::fs::remove_file(&index);
    let tree = tree?;

    let mut commit_args = vec![
        "commit-tree".to_string(),
        tree,
        "-m".to_string(),
        format!("pillbox collect: session {session}"),
    ];
    // Parent on base only if it's a real commit here; otherwise an orphan
    // commit (the orchestrator still gets *theirs*, just not a 3-way base).
    if let Some(b) = base.filter(|b| git_commit_exists(repo_root, b)) {
        commit_args.push("-p".to_string());
        commit_args.push(b.to_string());
    }
    let commit_args: Vec<&str> = commit_args.iter().map(String::as_str).collect();
    let commit = git_out(repo_root, None, None, true, &commit_args)?;

    let refname = format!("refs/pillbox/collect/{session}");
    git_out(
        repo_root,
        None,
        None,
        false,
        &["update-ref", &refname, &commit],
    )?;
    Ok(refname)
}

/// Build a git tree object from `result_dir` through `index`. A nested `.git`
/// (the workspace was itself a repo — the common case) is moved outside the
/// work-tree for the scan, then restored, so it's neither tracked nor mistaken
/// for a submodule gitlink.
fn build_result_tree(repo_root: &Path, result_dir: &Path, index: &Path) -> Result<String> {
    let nested_git = result_dir.join(".git");
    let aside = result_dir.with_extension("git-aside");
    let stashed = nested_git.exists();
    if stashed {
        std::fs::rename(&nested_git, &aside)
            .map_err(|e| PillboxError::runtime("collect", format!("stash {nested_git:?}: {e}")))?;
    }
    let tree = (|| {
        git_out(
            repo_root,
            Some(result_dir),
            Some(index),
            false,
            &["add", "--all"],
        )?;
        git_out(repo_root, None, Some(index), false, &["write-tree"])
    })();
    if stashed {
        std::fs::rename(&aside, &nested_git).map_err(|e| {
            PillboxError::runtime("collect", format!("restore {nested_git:?}: {e}"))
        })?;
    }
    tree
}

fn git_commit_exists(repo_root: &Path, sha: &str) -> bool {
    git_out(
        repo_root,
        None,
        None,
        false,
        &["cat-file", "-e", &format!("{sha}^{{commit}}")],
    )
    .is_ok()
}

/// Run `git -C repo_root [args]`, erroring on a non-zero exit (stderr folded in)
/// and returning trimmed stdout. `work_tree`/`index` set `--work-tree` /
/// `GIT_INDEX_FILE`; `ident` pins a pillbox author+committer so `commit-tree`
/// never depends on the repo's user config.
fn git_out(
    repo_root: &Path,
    work_tree: Option<&Path>,
    index: Option<&Path>,
    ident: bool,
    args: &[&str],
) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_root);
    if let Some(wt) = work_tree {
        cmd.arg("--work-tree").arg(wt);
    }
    if let Some(idx) = index {
        cmd.env("GIT_INDEX_FILE", idx);
    }
    if ident {
        cmd.env("GIT_AUTHOR_NAME", "pillbox")
            .env("GIT_AUTHOR_EMAIL", "collect@pillbox.local")
            .env("GIT_COMMITTER_NAME", "pillbox")
            .env("GIT_COMMITTER_EMAIL", "collect@pillbox.local");
    }
    let out = cmd.args(args).output().map_err(|e| {
        PillboxError::resource("collect", format!("invoke git: {e} (is git installed?)"))
    })?;
    if !out.status.success() {
        return Err(PillboxError::runtime(
            "collect",
            format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::session::{self, Session};
    use crate::test_util::with_isolated_home;

    #[test]
    fn entry_json_renders_nulls_for_absent_handles() {
        let e = CollectEntry {
            session: "abc123def456".into(),
            base_snapshot: None,
            base_git_anchor: None,
            result_snapshot: Some("f".repeat(64)),
            result_git_anchor: Some("a".repeat(40)),
            dir: PathBuf::from("/tmp/collected/abc123def456"),
            source: "snapshot",
            git_ref: None,
        };
        let v = e.to_json();
        assert_eq!(v["session"], "abc123def456");
        assert!(v["base_snapshot"].is_null());
        assert!(v["base_git_anchor"].is_null());
        assert_eq!(v["result_snapshot"], "f".repeat(64));
        assert_eq!(v["result_git_anchor"], "a".repeat(40));
        assert_eq!(v["dir"], "/tmp/collected/abc123def456");
        assert_eq!(v["source"], "snapshot");
        assert!(v["ref"].is_null(), "ref is null without --as-refs");
    }

    #[test]
    fn collect_errors_all_or_nothing_when_a_session_has_no_result() {
        with_isolated_home("collect-pending", || {
            let g = pillbox::global();
            // A fixture session with no result_snapshot and (docker, no live
            // backend) no live clone → unfinished.
            let mut s = Session::test_fixture();
            s.id = Session::new_id();
            s.result_snapshot = None;
            session::write(&g, &s).unwrap();

            let err = collect(&g, vec![s.id.clone()], None, false, true)
                .unwrap_err()
                .to_string();
            assert!(err.contains("no result yet"), "got: {err}");
            assert!(err.contains(&s.id), "error should name the laggard: {err}");
        });
    }

    #[test]
    fn collect_rejects_malformed_session_id_before_touching_fs() {
        with_isolated_home("collect-badid", || {
            let g = pillbox::global();
            // 12 chars but non-hex (X/Y/Z) — a corrupted/hand-edited record. The
            // path-traversal guard must trip before `to_dir.join(&id)`, even
            // though this record claims a result.
            let mut s = Session::test_fixture();
            s.id = "deadbeefXYZ1".into();
            s.result_snapshot = Some("f".repeat(64));
            session::write(&g, &s).unwrap();

            let err = collect(&g, vec![s.id.clone()], None, false, true)
                .unwrap_err()
                .to_string();
            assert!(err.contains("malformed"), "got: {err}");
        });
    }

    #[test]
    fn write_result_ref_builds_a_base_parented_commit_skipping_nested_git() {
        // git-only (no rustic), so fast. Skip where git isn't installed.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@example"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.path().join("a.txt"), b"base").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "base"]);
        let base_sha = g(&["rev-parse", "HEAD"]);

        // A result dir shaped like a worker's workspace: changed a.txt, a new
        // b.txt, AND a nested .git (must be skipped, not tracked/submoduled).
        let work = tempfile::tempdir().unwrap();
        let result_dir = work.path().join("result");
        std::fs::create_dir_all(result_dir.join(".git")).unwrap();
        std::fs::write(result_dir.join(".git").join("HEAD"), b"ref: refs/heads/x").unwrap();
        std::fs::write(result_dir.join("a.txt"), b"changed").unwrap();
        std::fs::write(result_dir.join("b.txt"), b"new").unwrap();

        let refname =
            write_result_ref(repo.path(), "aaaaaaaaaaaa", &result_dir, Some(&base_sha)).unwrap();
        assert_eq!(refname, "refs/pillbox/collect/aaaaaaaaaaaa");

        // Parent is the base commit → base→ref diffs to the worker's changes.
        assert_eq!(g(&["rev-parse", &format!("{refname}^")]), base_sha);

        // Tree carries the files, NOT the nested .git.
        let files = g(&["ls-tree", "-r", "--name-only", &refname]);
        assert!(files.lines().any(|l| l == "a.txt"), "{files}");
        assert!(files.lines().any(|l| l == "b.txt"), "{files}");
        assert!(
            !files.contains(".git"),
            "nested .git leaked into the ref tree: {files}"
        );
        assert_eq!(
            g(&["cat-file", "-p", &format!("{refname}:a.txt")]),
            "changed"
        );

        // The nested .git was restored in the result dir.
        assert!(
            result_dir.join(".git").join("HEAD").is_file(),
            "nested .git not restored after tree build"
        );
    }

    #[test]
    fn write_result_ref_makes_an_orphan_commit_when_base_absent() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@example"]);
        g(&["config", "user.name", "t"]);

        let work = tempfile::tempdir().unwrap();
        let result_dir = work.path().join("result");
        std::fs::create_dir_all(&result_dir).unwrap();
        std::fs::write(result_dir.join("only.txt"), b"x").unwrap();

        // base is a sha that doesn't exist in this repo → no parent, no error.
        let refname = write_result_ref(
            repo.path(),
            "bbbbbbbbbbbb",
            &result_dir,
            Some(&"0".repeat(40)),
        )
        .unwrap();
        let parents = g(&["rev-list", "--parents", "-n", "1", &refname]);
        // `rev-list --parents` prints "<commit>" with no parents for an orphan.
        assert_eq!(
            parents.split_whitespace().count(),
            1,
            "expected an orphan commit, got parents: {parents}"
        );
    }
}
