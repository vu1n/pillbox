//! Grading a worker's tree — the one path `dispatch` and `eval` both take.
//!
//! A worker's live workspace is untrusted: it is frozen into a snapshot, copied
//! into a [`GradeTree`] with secrets scrubbed and protected paths restored from a
//! trusted base, and scored only inside the one-shot grader microVM (never on the
//! host). The session calls go through [`SessionOps`] so tests can assert those
//! properties without booting a VM.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::commands::session::ScoreRequest;
use crate::contract::Scored;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::workspace::protect::{
    force_remove, snapshot_workspace, GradeTree, ProtectSet, GRADER_FILES_DIR,
};
use crate::workspace::WorkspaceBackend;

/// The session calls a grade and a winner pull go through. The live
/// [`LiveSessions`] is the session API; tests substitute a recorder so the
/// grading properties (VM only, the scrubbed snapshot with protected paths
/// restored, no host fallback) are asserted without booting a VM.
pub(crate) trait SessionOps {
    /// The live workspace directory of session `id` (libkrun sessions only).
    fn workspace(&self, resolved: &Pillbox, id: &str) -> Result<PathBuf>;
    /// Grade per `request` and record the verdict on the session's §0 log.
    fn score(&self, resolved: &Pillbox, id: &str, request: &ScoreRequest) -> Result<Scored>;
    /// Rehydrate session `id`'s result tree into `to`.
    fn pull(&self, resolved: &Pillbox, id: &str, to: &Path) -> Result<()>;
}

/// [`SessionOps`] over the session API — what `dispatch` and `eval` run with.
pub(crate) struct LiveSessions;

impl SessionOps for LiveSessions {
    fn workspace(&self, resolved: &Pillbox, id: &str) -> Result<PathBuf> {
        let session = crate::session::resolve(resolved, id)?;
        crate::commands::session::workspace_of(&session)
            .with_context(|| format!("workspace of worker `{id}` (libkrun-only today)"))
    }

    fn score(&self, resolved: &Pillbox, id: &str, request: &ScoreRequest) -> Result<Scored> {
        Ok(crate::commands::session::score(resolved, id, request)?.scored)
    }

    fn pull(&self, resolved: &Pillbox, id: &str, to: &Path) -> Result<()> {
        let session = crate::session::resolve(resolved, id)?;
        crate::commands::session::rehydrate_result(resolved, &session, to, "dispatch")?;
        Ok(())
    }
}

/// Whether this build can run the grader microVM. Without it dispatch and eval
/// refuse to start rather than grade worker code on the host.
pub(crate) fn grader_vm_available() -> bool {
    crate::sandbox::SubstrateKind::Libkrun
        .backend()
        .is_ok_and(|backend| backend.capabilities().in_sandbox_grading)
}

/// Restore `bookmark` into a fresh temp dir and scrub it like a worker's clone —
/// never the warm base cache the workers clone from.
pub(crate) fn restore_bookmark(
    resolved: &Pillbox,
    bookmark: &str,
    purpose: &str,
) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("pillbox-grade-base-")
        .tempdir()
        .with_context(|| format!("temp dir for {purpose}"))?;
    let handle = crate::bookmarks::resolve_existing(resolved, bookmark)?;
    resolved.workspace()?.pull(dir.path(), Some(&handle))?;
    crate::workspace::ingest::scrub_secrets(dir.path())?;
    Ok(dir)
}

/// Copy the host directory every worker is cloned from into a fresh temp dir,
/// scrubbed like a worker's clone: the protected-path base when runs start from a
/// workspace directory rather than a bookmark.
pub(crate) fn copy_base_dir(src: &Path, purpose: &str) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("pillbox-grade-base-")
        .tempdir()
        .with_context(|| format!("temp dir for {purpose}"))?;
    let tree = dir.path().join("ws");
    crate::workspace::cow::cow_clone_dir(src, &tree)
        .with_context(|| format!("copy {} for {purpose}", src.display()))?;
    crate::workspace::ingest::scrub_secrets(&tree)?;
    Ok(dir)
}

/// The score request that grades `tree` inside the one-shot grader microVM.
// The reward is an external grader's verdict; a worker must not be able to edit or run code outside the VM to change it.
pub(crate) fn grade_request<'r>(
    tree: &'r Path,
    cmd: Option<&'r str>,
    rubric: Option<&'r Path>,
    egress: &'r [String],
    drop_privileges: bool,
) -> ScoreRequest<'r> {
    ScoreRequest {
        cmd,
        rubric,
        snapshot: None,
        workspace: Some(tree),
        // Worker trees grade only in the grader microVM, never on the host.
        in_sandbox: true,
        grader_egress: egress,
        drop_privileges,
    }
}

/// What a worker grade is checked against, fixed for a whole dispatch or eval.
pub(crate) struct GradePolicy<'a> {
    /// The trusted tree protected paths are restored from.
    pub(crate) base: &'a Path,
    pub(crate) protect: &'a ProtectSet,
    /// Hidden grader files injected into the grade tree.
    pub(crate) grader_files: Option<&'a Path>,
    /// Hosts the grader microVM may reach.
    pub(crate) egress: &'a [String],
    /// The command grading (`dispatch`/`eval`), for the stderr note.
    pub(crate) command: &'static str,
}

/// The protected-path knobs of a grade, as `dispatch` takes them in flags and
/// `eval` in its spec's `[verify]` table.
pub(crate) struct GradeKnobs<'a> {
    /// Extra protected path patterns, relative to the workspace.
    pub(crate) protect: &'a [String],
    /// Drop [`crate::workspace::protect::DEFAULT_PROTECTED`].
    pub(crate) no_default_protect: bool,
    /// Hidden grader files injected into every grade tree.
    pub(crate) grader_files: Option<&'a Path>,
}

/// How the caller's user spells each knob, so a usage error names what they wrote.
pub(crate) struct KnobNames {
    pub(crate) command: &'static str,
    pub(crate) protect: &'static str,
    pub(crate) grader_files: &'static str,
    /// An example extra protected pattern, in the caller's syntax.
    pub(crate) protect_example: &'static str,
}

/// Validate `knobs` and build the protected set every grade restores: the
/// defaults (unless dropped) plus the extra patterns, and, with grader files,
/// their `.pillbox-grader/` landing spot.
pub(crate) fn grade_protect_set(
    clone_source: &Path,
    knobs: &GradeKnobs,
    names: &KnobNames,
) -> Result<ProtectSet> {
    let mut protect_patterns = knobs.protect.to_vec();
    if let Some(dir) = knobs.grader_files {
        if !dir.is_dir() {
            return Err(PillboxError::usage(
                names.command,
                format!(
                    "{} {} is not a directory",
                    names.grader_files,
                    dir.display()
                ),
            )
            .into());
        }
        // Hidden only if the clone can't carry it: a directory inside the
        // clone source ships to every worker with the rest of the tree.
        if let Some(project) = grader_files_inside(dir, clone_source) {
            return Err(PillboxError::usage(
                names.command,
                format!(
                    "{} {} is inside the project {}, so workers can read it",
                    names.grader_files,
                    dir.display(),
                    project.display()
                ),
            )
            .with_next(
                "keep hidden checks outside the project directory, e.g. ../myapp-hidden-tests/",
            )
            .into());
        }
        // A worker can't plant its own `.pillbox-grader/`: the landing spot is protected (and locked).
        protect_patterns.push(format!("/{GRADER_FILES_DIR}/"));
    }
    ProtectSet::new(!knobs.no_default_protect, &protect_patterns).map_err(|e| {
        PillboxError::usage(names.command, format!("{}: {e:#}", names.protect))
            .with_next(format!(
                "patterns are paths relative to the workspace, e.g. {}",
                names.protect_example
            ))
            .into()
    })
}

/// The directory dispatch's workers fork from: the resolved pillbox's own
/// directory (the one holding its `pillbox.toml`, whatever the cwd), or, on the
/// global pillbox, the nearest ancestor of the cwd holding `pillbox.toml` or
/// `.git` (else the cwd itself).
pub(crate) fn dispatch_clone_source(resolved: &Pillbox) -> PathBuf {
    if let crate::pillbox::Scope::Project { source_dir, .. } = &resolved.scope {
        return source_dir.clone();
    }
    let cwd = std::env::current_dir()
        .and_then(|c| c.canonicalize())
        .unwrap_or_default();
    cwd.ancestors()
        .find(|a| a.join("pillbox.toml").exists() || a.join(".git").exists())
        .unwrap_or(&cwd)
        .to_path_buf()
}

/// `clone_source` (canonical) if `dir` sits inside it; unresolvable paths pass.
fn grader_files_inside(dir: &Path, clone_source: &Path) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    let source = clone_source.canonicalize().ok()?;
    dir.starts_with(&source).then_some(source)
}

/// Grade worker `id`: freeze its live workspace into `snap` (left in place, so a
/// caller can deliver exactly the graded tree), prepare a [`GradeTree`] from it
/// with protected paths restored from `policy.base`, and score that inside the
/// grader microVM. A VM that can't run fails the score: there is no host fallback.
pub(crate) fn grade_worker(
    ops: &dyn SessionOps,
    resolved: &Pillbox,
    id: &str,
    snap: &Path,
    cmd: Option<&str>,
    rubric: Option<&Path>,
    policy: &GradePolicy,
) -> Result<Scored> {
    let ws = ops.workspace(resolved, id)?;
    if snap.exists() {
        force_remove(snap)?;
    }
    snapshot_workspace(&ws, snap)?;
    let tree = GradeTree::prepare(snap, policy.base, policy.protect, policy.grader_files)?;
    if let Some(note) = reverted_note(policy.command, id, &tree.report.reverted) {
        eprintln!("{note}");
    }
    let request = grade_request(tree.path(), cmd, rubric, policy.egress, tree.locked);
    ops.score(resolved, id, &request)
}

/// Stderr note for a worker whose grade tree had protected paths put back.
pub(crate) fn reverted_note(command: &str, id: &str, reverted: &[PathBuf]) -> Option<String> {
    const SHOWN: usize = 5;
    if reverted.is_empty() {
        return None;
    }
    let mut list: Vec<String> = reverted
        .iter()
        .take(SHOWN)
        .map(|p| p.display().to_string())
        .collect();
    if reverted.len() > SHOWN {
        list.push(format!("+{} more", reverted.len() - SHOWN));
    }
    Some(format!(
        "pillbox: {command}: worker {id} changed protected paths; graded with the base's copy: {}",
        list.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grader_files_inside_the_clone_source_are_refused() {
        let source = tempfile::TempDir::new().unwrap();
        let hidden = source.path().join("hidden");
        std::fs::create_dir(&hidden).unwrap();
        assert!(grader_files_inside(&hidden, source.path()).is_some());
        let outside = tempfile::TempDir::new().unwrap();
        assert!(grader_files_inside(outside.path(), source.path()).is_none());
    }
}

/// The recording [`SessionOps`] stand-in dispatch's and eval's grading tests share.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use anyhow::bail;
    use std::cell::RefCell;

    /// What the grader-VM stand-in does with the tree it is handed.
    #[derive(Clone, Copy)]
    pub(crate) enum Vm {
        /// Runs the grader against the tree, with an empty env, and reports the verdict.
        RunsGrader,
        /// Executes nothing and reports a failing grade.
        RecordsOnly,
        /// The grader VM can't run.
        Down,
    }

    /// One `score` the driver asked for, kept as owned data.
    #[derive(Debug, Clone)]
    pub(crate) struct ScoreCall {
        pub(crate) id: String,
        pub(crate) workspace: Option<PathBuf>,
        pub(crate) snapshot: Option<String>,
        pub(crate) in_sandbox: bool,
        pub(crate) drop_privileges: bool,
        pub(crate) egress: Vec<String>,
        pub(crate) cmd: Option<String>,
    }

    /// The session API standing in under [`SessionDriver`]: `workspace` reports
    /// `ws` as the worker's live workspace; `score` records the request, refuses
    /// to grade without `in_sandbox`, copies the tree it was handed to `seen`
    /// (the grade tree is gone once `grade` returns), then does what `vm` says.
    /// Every `pull` is recorded too.
    pub(crate) struct FakeSessions {
        pub(crate) _dir: tempfile::TempDir,
        pub(crate) ws: PathBuf,
        pub(crate) vm: Vm,
        pub(crate) seen: PathBuf,
        pub(crate) scores: RefCell<Vec<ScoreCall>>,
        pub(crate) pulls: RefCell<Vec<String>>,
    }

    impl FakeSessions {
        pub(crate) fn new(ws: &Path, vm: Vm) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let seen = dir.path().join("seen");
            Self {
                _dir: dir,
                ws: ws.to_path_buf(),
                vm,
                seen,
                scores: RefCell::new(Vec::new()),
                pulls: RefCell::new(Vec::new()),
            }
        }

        pub(crate) fn scores(&self) -> Vec<ScoreCall> {
            self.scores.borrow().clone()
        }
    }

    impl SessionOps for FakeSessions {
        fn workspace(&self, _: &Pillbox, _: &str) -> Result<PathBuf> {
            Ok(self.ws.clone())
        }

        fn score(&self, _: &Pillbox, id: &str, request: &ScoreRequest) -> Result<Scored> {
            self.scores.borrow_mut().push(ScoreCall {
                id: id.into(),
                workspace: request.workspace.map(Path::to_path_buf),
                snapshot: request.snapshot.map(str::to_string),
                in_sandbox: request.in_sandbox,
                drop_privileges: request.drop_privileges,
                egress: request.grader_egress.to_vec(),
                cmd: request.cmd.map(str::to_string),
            });
            if !request.in_sandbox {
                bail!("host grading refused");
            }
            let tree = request.workspace.context("no tree to grade")?;
            let _ = std::fs::remove_dir_all(&self.seen);
            let copied = std::process::Command::new("cp")
                .arg("-R")
                .arg(tree)
                .arg(&self.seen)
                .status()
                .unwrap();
            assert!(copied.success());
            let passed = match self.vm {
                Vm::Down => bail!("the grader microVM exited (1) before the grader started"),
                Vm::RecordsOnly => false,
                Vm::RunsGrader => std::process::Command::new("env")
                    .args(["-i", "PATH=/usr/bin:/bin", "sh", "-c"])
                    .arg(request.cmd.unwrap_or("false"))
                    .current_dir(tree)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success()),
            };
            Ok(Scored {
                grader: "test".into(),
                passed,
                score: if passed { 1.0 } else { 0.0 },
                feedback: String::new(),
                criteria: vec![],
            })
        }

        fn pull(&self, _: &Pillbox, id: &str, to: &Path) -> Result<()> {
            self.pulls.borrow_mut().push(id.into());
            std::fs::create_dir_all(to)?;
            Ok(())
        }
    }

    pub(crate) fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// The bookmark (`base`) and a worker workspace forked from it (`ws`).
    /// The base test passes only once `src/lib.rs` contains `fixed`.
    pub(crate) struct Project {
        pub(crate) base: tempfile::TempDir,
        pub(crate) _root: tempfile::TempDir,
        pub(crate) ws: PathBuf,
    }

    pub(crate) fn project() -> Project {
        let base = tempfile::tempdir().unwrap();
        put(base.path(), "src/lib.rs", "pub fn f() {}\n");
        put(base.path(), "tests/check.sh", "grep -q fixed src/lib.rs\n");
        let root = tempfile::tempdir().unwrap();
        let ws = root.path().join("ws");
        crate::workspace::cow::cow_clone_dir(base.path(), &ws).unwrap();
        Project {
            base,
            _root: root,
            ws,
        }
    }

    pub(crate) fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }
}
