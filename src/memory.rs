//! Host-side integration with `kypp` — the external swarm-memory engine (github.com/vu1n/kypp).
//!
//! `pillbox run --memory` ATTACHES to kypp, it doesn't own it: pillbox shells out to the `kypp` CLI
//! to brief the agent from project memory at session start and to capture the §0 log after the run.
//! Best-effort by design — a missing or erroring `kypp` prints a note and the run proceeds; memory
//! is an enhancement, never a gate. Scope is `KYPP_PROJECT` = the pillbox name. Recall/capture are
//! kypp verbs (this module); the optional mid-task MCP attach (`--mcp kypp=<url>`) is the generic,
//! provider-agnostic path (mem0 etc. drop in there) and needs no code here.
use std::process::Command;

/// Brief the agent from project memory by PREPENDING kypp's session-start digest to the run's single
/// positional prompt (the `pillbox run -- "task"` shape). Skips when the shape is ambiguous — 0
/// positionals (a bare interactive run) silently, >1 (flags after `--`) with a note — since there's
/// no unambiguous prompt to prepend to; mid-task recall (the MCP attach) covers those cases.
pub(crate) fn brief_into_args(args: &mut [String], project: &str) {
    match positionals(args).as_slice() {
        [i] => {
            if let Some(digest) = briefing(project) {
                args[*i] = format!(
                    "## Project memory (kypp)\n{digest}\n\n## Task\n{}",
                    args[*i]
                );
            }
        }
        [] => {} // bare interactive run — nothing to prepend to; mid-task recall covers it
        more => eprintln!(
            "pillbox: note: --memory briefing skipped ({} positional args; needs one prompt)",
            more.len()
        ),
    }
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

/// `kypp sweep` for `project` — capture completed sessions (this run's §0 log included) into memory.
/// Idempotent on kypp's side; fully best-effort here.
pub(crate) fn sweep(project: &str) {
    let _ = run_kypp(&["sweep"], project);
}

fn run_kypp(args: &[&str], project: &str) -> Option<String> {
    match Command::new("kypp")
        .args(args)
        .env("KYPP_PROJECT", project)
        .output()
    {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => {
            eprintln!(
                "pillbox: note: `kypp {}` exited {} — memory step skipped",
                args[0], o.status
            );
            None
        }
        Err(_) => {
            eprintln!(
                "pillbox: note: `kypp` not found — --memory step skipped. Install: github.com/vu1n/kypp"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::positionals;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn positionals_isolates_the_single_prompt() {
        assert_eq!(positionals(&v(&["task"])), vec![0]); // `pillbox run -- "task"` → inject
        assert_eq!(positionals(&v(&[])), Vec::<usize>::new()); // bare interactive → skip silently
                                                               // flags after `--` make the prompt ambiguous (>1 positional) → skip with a note
        assert_eq!(
            positionals(&v(&["--permission-mode", "plan", "task"])),
            vec![1, 2]
        );
    }
}
