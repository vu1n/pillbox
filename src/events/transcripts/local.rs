//! Host-side transcript streaming for local-docker foreground runs.
//!
//! Unlike the remote backends — where the transcript jsonl lives
//! inside an ephemeral sandbox — local-docker bind-mounts the agent's
//! `$HOME` from the host (see `sandbox::local_docker`). So the
//! harness's `~/.claude/projects/<encoded>/<uuid>.jsonl` (Codex:
//! `~/.codex/sessions/.../rollout-*.jsonl`) is written straight to a
//! host path, and the host can tail it live and ship spans to the
//! operator's collector with no egress hop.
//!
//! The uuid isn't known before the agent starts, so discovery is:
//! snapshot the existing `*.jsonl` under the harness's transcript root
//! before launch, then poll for the first file that wasn't there. The
//! freshly-started agent creates exactly one. Once found, the standard
//! [`Tailer`] drives it in follow mode until the agent exits, at which
//! point dropping the [`LocalTailerHandle`] (or calling
//! [`LocalTailerHandle::shutdown`]) flips the stop flag and joins —
//! a final drain catches the agent's last lines.
//!
//! Concurrency: the agent's `$HOME` is the *shared* global auth dir, so
//! every pillbox run of the same agent writes under one transcript
//! tree. `scope_dir` (Claude: this run's own `-workspace-<name>` dir)
//! narrows discovery so two concurrent runs don't grab each other's
//! file. Codex buckets by date with no per-run dir, so concurrent Codex
//! runs can still race — rare, and documented on [`Harness::transcript_roots`].
//!
//! Assumption: a default `pillbox run` starts a *fresh* harness
//! session (new uuid file). A future `--continue`/`--resume` that
//! re-opens a prior transcript would land in the pre-launch snapshot
//! and be skipped — acceptable for v1; revisit if resume lands.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::{Harness, Tailer};

/// Owns the background tailer thread for one local foreground run.
/// Stopping is idempotent and happens on either [`shutdown`](Self::shutdown)
/// or `Drop` — both flip the stop flag and join, and the joined thread
/// does one final pump on its way out, so the agent's last appended
/// lines are emitted regardless of which path runs. `shutdown` exists
/// only to make the stop point explicit at the call site (and force the
/// drop there rather than at end of scope).
pub(crate) struct LocalTailerHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl LocalTailerHandle {
    pub(crate) fn shutdown(self) {
        // Drop does the work; this just names the intent + forces it here.
    }

    /// Flip the stop flag and join the tailer thread. Idempotent via
    /// `join.take()`, so a later `Drop` after `shutdown` is a no-op.
    /// The thread observes `stop` within one poll interval, does a
    /// final drain, and exits; a tailer still in discovery (agent
    /// exited before writing a transcript) returns immediately.
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for LocalTailerHandle {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Spawn a background thread that discovers the freshly-created
/// transcript under `watch_root` and tails it as OTLP child spans of
/// `session_id`'s session span. Returns immediately; the caller drives
/// the lifecycle via the returned handle.
///
/// `watch_root` is the harness's transcript directory on the host
/// (e.g. `<home>/.claude/projects` or `<home>/.codex/sessions`) — it
/// may not exist yet when this is called; discovery polls until it
/// does and a new file appears. `scope_dir`, when `Some`, restricts
/// discovery to this run's own transcript directory (see module docs);
/// `None` discovers across the whole `watch_root` tree.
pub(crate) fn spawn_local_tailer(
    watch_root: PathBuf,
    scope_dir: Option<PathBuf>,
    harness: Harness,
    session_id: String,
    include_usage: bool,
) -> LocalTailerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    // Snapshot pre-existing transcripts so we tail only the one this
    // run creates, not a prior session's file in the same project dir.
    let preexisting = snapshot_jsonl(&watch_root);

    let join = std::thread::spawn(move || {
        let path = loop {
            if stop_thread.load(Ordering::Relaxed) {
                return; // asked to stop before the agent wrote anything
            }
            if let Some(p) = find_new_jsonl(&watch_root, scope_dir.as_deref(), &preexisting) {
                break p;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let mut tailer = Tailer::new(path, session_id, harness, include_usage);
        if let Err(e) = tailer.follow_until(&stop_thread) {
            eprintln!("pillbox: warning: transcript tailer stopped: {e:#}");
        }
    });

    LocalTailerHandle {
        stop,
        join: Some(join),
    }
}

/// All `*.jsonl` paths under `root` (recursive). Empty if `root`
/// doesn't exist or can't be read — discovery tolerates a not-yet-
/// created transcript dir.
fn snapshot_jsonl(root: &Path) -> HashSet<PathBuf> {
    let mut out = Vec::new();
    collect_jsonl(root, &mut out);
    out.into_iter().collect()
}

/// The most-recently-modified `*.jsonl` under `root` that isn't in
/// `exclude`. When `scope_dir` is `Some`, only files under it are
/// considered — and we deliberately do NOT fall back to the wider
/// `root` if it's empty, so a concurrent run's file in a sibling dir
/// can never be mis-attributed (the cost is no thread spans if the
/// scope dir name is ever computed wrong — preferable to wrong spans).
///
/// Files whose mtime can't be read are skipped (not demoted to
/// "oldest"), so a transient `stat` error on the real transcript
/// can't latch discovery onto an unrelated file. Ties break on path
/// for determinism across platforms' unspecified `read_dir` order.
fn find_new_jsonl(
    root: &Path,
    scope_dir: Option<&Path>,
    exclude: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    collect_jsonl(root, &mut candidates);
    candidates
        .into_iter()
        .filter(|p| !exclude.contains(p))
        .filter(|p| scope_dir.is_none_or(|d| p.starts_with(d)))
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((mtime, p))
        })
        .max_by(|(ta, pa), (tb, pb)| ta.cmp(tb).then_with(|| pa.cmp(pb)))
        .map(|(_, p)| p)
}

/// Recursively append every `*.jsonl` file under `dir` to `out`.
/// Skips unreadable subdirectories silently (best-effort discovery).
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect_jsonl(&path, out);
        } else if ft.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::*;

    #[test]
    fn find_new_jsonl_ignores_preexisting_and_picks_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects").join("-workspace-app");
        fs::create_dir_all(&root).unwrap();

        let old = root.join("old-session.jsonl");
        fs::File::create(&old).unwrap();
        let pre = snapshot_jsonl(dir.path());
        assert!(pre.contains(&old));

        // No new file yet.
        assert_eq!(find_new_jsonl(dir.path(), None, &pre), None);

        // Agent writes a new transcript.
        let fresh = root.join("new-session.jsonl");
        let mut f = fs::File::create(&fresh).unwrap();
        writeln!(f, "{{}}").unwrap();

        assert_eq!(find_new_jsonl(dir.path(), None, &pre), Some(fresh));
    }

    #[test]
    fn find_new_jsonl_scope_dir_ignores_sibling_runs_transcript() {
        // Two concurrent runs share the projects tree; each must only
        // pick up its own scope dir's file, never the sibling's.
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("-workspace-app-a");
        let theirs = dir.path().join("-workspace-app-b");
        fs::create_dir_all(&mine).unwrap();
        fs::create_dir_all(&theirs).unwrap();

        let pre = snapshot_jsonl(dir.path()); // empty
                                              // Sibling run's file appears first (newest mtime globally).
        fs::File::create(theirs.join("sibling.jsonl")).unwrap();
        // Scoped to my dir: sibling is invisible, so nothing yet.
        assert_eq!(find_new_jsonl(dir.path(), Some(&mine), &pre), None);

        // My file appears; scoped discovery finds only it.
        let my_file = mine.join("mine.jsonl");
        fs::File::create(&my_file).unwrap();
        assert_eq!(find_new_jsonl(dir.path(), Some(&mine), &pre), Some(my_file));
    }

    #[test]
    fn collect_jsonl_recurses_and_filters_extension() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        fs::File::create(nested.join("keep.jsonl")).unwrap();
        fs::File::create(nested.join("skip.txt")).unwrap();
        fs::File::create(dir.path().join("top.jsonl")).unwrap();

        let mut out = Vec::new();
        collect_jsonl(dir.path(), &mut out);
        let names: HashSet<_> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains("keep.jsonl"));
        assert!(names.contains("top.jsonl"));
        assert!(!names.contains("skip.txt"));
    }

    #[test]
    fn snapshot_of_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(snapshot_jsonl(&missing).is_empty());
    }
}
