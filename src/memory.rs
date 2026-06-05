//! Host-side integration with `kypp` — the external swarm-memory engine (github.com/vu1n/kypp).
//!
//! `pillbox run --memory` ATTACHES to kypp, it doesn't own it: pillbox shells out to the `kypp` CLI
//! to brief the agent from project memory at session start and to capture the §0 log after the run.
//! Best-effort by design — a missing or erroring `kypp` prints a note and the run proceeds; memory
//! is an enhancement, never a gate. Scope is `KYPP_PROJECT` = the pillbox name. Recall/capture are
//! kypp verbs (this module); the optional mid-task MCP attach (`--mcp kypp=<url>`) is the generic,
//! provider-agnostic path (mem0 etc. drop in there) and needs no code here.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Brief the agent from project memory by PREPENDING kypp's session-start digest to the run's single
/// positional prompt (the `pillbox run -- "task"` shape). Skips when the shape is ambiguous — 0
/// positionals (a bare interactive run) silently, >1 (flags after `--`) with a note — since there's
/// no unambiguous prompt to prepend to; mid-task recall (the MCP attach) covers those cases.
/// Returns the claim handles it briefed (for usage provenance), empty on every skip path.
pub(crate) fn brief_into_args(args: &mut [String], project: &str) -> Vec<String> {
    match positionals(args).as_slice() {
        [i] => {
            if let Some(digest) = briefing(project) {
                let handles = parse_handles(&digest);
                args[*i] = format!(
                    "## Project memory (kypp)\n{digest}\n\n## Task\n{}",
                    args[*i]
                );
                handles
            } else {
                Vec::new()
            }
        }
        [] => Vec::new(), // bare interactive run — nothing to prepend to; mid-task recall covers it
        more => {
            eprintln!(
                "pillbox: note: --memory briefing skipped ({} positional args; needs one prompt)",
                more.len()
            );
            Vec::new()
        }
    }
}

/// The claim handles in a `kypp briefing` digest: per non-empty line, the first whitespace-delimited
/// token, kept only if it has the compact-line handle shape (lowercase hex, ≥4 chars). Lines that
/// don't lead with a handle (blanks, headers, prose) contribute nothing.
fn parse_handles(digest: &str) -> Vec<String> {
    digest
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|tok| {
            tok.len() >= 4
                && tok
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        .map(str::to_string)
        .collect()
}

/// Indices of the non-flag args (the prompt candidates). The prompt is unambiguous only when there's
/// exactly one — flags after `--` (their values look positional too) yield >1, so we bail there.
fn positionals(args: &[String]) -> Vec<usize> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| !a.starts_with('-'))
        .map(|(i, _)| i)
        .collect()
}

/// `kypp briefing` for `project` → its digest (compact handle lines), or None when kypp is
/// absent/errors/empty.
fn briefing(project: &str) -> Option<String> {
    let out = run_kypp(&["briefing"], project)?;
    let digest = out.trim();
    (!digest.is_empty()).then(|| digest.to_string())
}

/// Capture THIS run's session(s) into kypp after the agent exits — the §0 logs under `sessions_root`
/// written since `since` (the run window). Per-session `kypp capture --distill`, NOT a blanket `kypp
/// sweep`: sweep would retroactively pull every uncaptured session in ~/.pillbox under this run's
/// project (a backlog flush + cross-project mis-attribution). Best-effort; kypp dedupes by marker.
pub(crate) fn capture_run(sessions_root: &Path, project: &str, since: SystemTime) {
    for log in run_window_logs(sessions_root, since) {
        if let Some(path) = log.to_str() {
            let _ = run_kypp(&["capture", path, "--distill"], project);
        }
    }
}

/// Record usage provenance for the claims this run was briefed with — one `kypp usage` row per
/// handle, per run-window session — so a later credit-assignment step can attribute the run's
/// verifiable score to the claims it saw. No-op when nothing was briefed. Best-effort (like
/// capture_run): a missing/erroring kypp just skips the row. Session id = the log's parent dir name.
pub(crate) fn record_brief_usage(
    sessions_root: &Path,
    project: &str,
    since: SystemTime,
    handles: &[String],
) {
    if handles.is_empty() {
        return;
    }
    for log in run_window_logs(sessions_root, since) {
        let Some(id) = log
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
        else {
            continue;
        };
        let mut args = vec![
            "usage".to_string(),
            "--record".to_string(),
            "--session".to_string(),
            id.to_string(),
            "--surface".to_string(),
            "briefing".to_string(),
        ];
        for h in handles {
            args.push("--claim".to_string());
            args.push(h.clone());
        }
        let _ = run_kypp_args(&args, project);
    }
}

/// The `<sessions_root>/<id>/log.jsonl` files modified at/after `since` — i.e. this run's, not the
/// pre-existing backlog. (mtime is the run-window signal; the log is finalized when the agent exits.)
fn run_window_logs(sessions_root: &Path, since: SystemTime) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path().join("log.jsonl"))
        .filter(|log| {
            std::fs::metadata(log)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t >= since)
        })
        .collect()
}

fn run_kypp(args: &[&str], project: &str) -> Option<String> {
    run_kypp_inner(args.iter().map(|s| s.as_ref()), args[0], project)
}

/// Owned/var-arg sibling of `run_kypp` (for callers that build args dynamically, like usage rows).
/// Shares the single spawn path so error-note shape stays identical.
fn run_kypp_args(args: &[String], project: &str) -> Option<String> {
    let verb = args.first().map(String::as_str).unwrap_or("");
    run_kypp_inner(args.iter().map(String::as_str), verb, project)
}

fn run_kypp_inner<'a>(
    args: impl IntoIterator<Item = &'a str>,
    verb: &str,
    project: &str,
) -> Option<String> {
    match Command::new("kypp")
        .args(args)
        .env("KYPP_PROJECT", project)
        .output()
    {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => {
            eprintln!(
                "pillbox: note: `kypp {verb}` exited {} — memory step skipped",
                o.status
            );
            None
        }
        Err(e) => {
            // Usually ENOENT (kypp not installed), but surface the real error — a permission/spawn
            // failure shouldn't masquerade as "not found".
            eprintln!("pillbox: note: `kypp` unavailable ({e}) — --memory skipped. Install: github.com/vu1n/kypp");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_handles, positionals, run_window_logs};
    use std::time::{Duration, SystemTime};

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn positionals_isolates_the_single_prompt() {
        // `pillbox run -- "task"` → one positional → inject
        assert_eq!(positionals(&v(&["task"])), vec![0]);
        // bare interactive (no prompt) → none → skip silently
        assert_eq!(positionals(&v(&[])), Vec::<usize>::new());
        // flags after `--` make it ambiguous (>1 positional) → skip with a note
        assert_eq!(
            positionals(&v(&["--permission-mode", "plan", "task"])),
            vec![1, 2]
        );
    }

    #[test]
    fn parse_handles_keeps_only_compact_handle_lines() {
        let digest = "\
a1b2c3 fixed the flaky retry test
deadbeef avoid global mutable config
this line has no leading handle
   \n\
00ff prefer explicit errors";
        // first token per line, kept iff it's lowercase hex ≥4 chars
        assert_eq!(
            parse_handles(digest),
            vec!["a1b2c3", "deadbeef", "00ff"],
            "prose/blank lines drop; handle-led lines keep their handle"
        );
        // a bare header / no-handle digest yields nothing
        assert_eq!(
            parse_handles("Project memory\n\nxyz notes"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn run_window_logs_picks_only_this_runs_session() {
        let root = std::env::temp_dir().join(format!("kypp-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |name: &str| {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("log.jsonl"), b"{}\n").unwrap();
        };
        mk("backlog"); // pre-existing session — must NOT be captured
        std::thread::sleep(Duration::from_millis(20));
        let started = SystemTime::now(); // the run window opens here
        std::thread::sleep(Duration::from_millis(20));
        mk("this_run"); // log written during the run → captured
        std::fs::create_dir_all(root.join("in_flight")).unwrap(); // no log.jsonl yet → ignored

        let got: Vec<String> = run_window_logs(&root, started)
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            got,
            vec!["this_run"],
            "only the run-window session, not the backlog"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
