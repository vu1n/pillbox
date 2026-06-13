//! `pillbox dispatch` — the worker-loop primitive (ghost's runtime fan-out).
//!
//! Fork `k` detached worker sessions from a snapshot bookmark, drive each to
//! idle on the same segment prompt, grade each with the rubric/cmd, retry
//! failures (feeding the failing criteria back as the next prompt), then select
//! the highest-scoring worker and pull its result workspace. Best-of-k turns
//! the long-horizon variance σ̂ into expected gain instead of a measurement
//! enemy — which is why per-fork diversity (`--temperature`) matters: `k`
//! identical deterministic workers all score the same and select-best buys
//! nothing.
//!
//! **This file is the CONTRACT (GHOST-002).** It defines the CLI surface, the
//! option/verdict types, and the JSON envelope that the loop implementation
//! (GHOST-003) and the live e2e (GHOST-004) program against. The handler here
//! is a stub: it validates the surface and reports unimplemented. See
//! `docs/dispatch.md`.

use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::json;

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;

/// Options for one `pillbox dispatch` invocation, populated from the clap
/// surface in `main.rs`. The grader is `cmd` xor `rubric` (a required clap
/// ArgGroup enforces exactly one at parse time, mirroring `session score`).
pub(crate) struct DispatchOpts {
    /// Snapshot bookmark every worker forks from — the shared base. Each worker
    /// is a `run --from-bookmark <name> --detach`.
    pub(crate) from_bookmark: String,
    /// How many parallel worker sessions to fork (`-k`). Must be ≥ 1.
    pub(crate) workers: u32,
    /// Grader: a single verifier command run via `sh -c` (exit 0 → pass / 1.0).
    /// Mutually exclusive with `rubric`.
    pub(crate) cmd: Option<String>,
    /// Grader: a rubric file (`NAME :: COMMAND` per line) → per-criterion
    /// verdicts + a fractional score. Mutually exclusive with `cmd`.
    pub(crate) rubric: Option<PathBuf>,
    /// Per-worker retry budget when the grade fails — the loop feeds the failing
    /// criteria back as the next prompt and re-grades, up to this many times.
    pub(crate) retries: u32,
    /// Worker agent (default: `pillbox.toml` `agent`, then `claude`).
    pub(crate) agent: Option<String>,
    /// Worker model override, forwarded to each worker's run.
    pub(crate) model: Option<String>,
    /// Per-fork sampling temperature, forwarded to each worker's run — the
    /// diversity knob that keeps best-of-`k` non-degenerate.
    pub(crate) temperature: Option<f64>,
    /// Wire in kypp swarm-memory (`--memory`) for each worker.
    pub(crate) memory: bool,
    /// The segment prompt handed to every worker (the positional `-- args`).
    pub(crate) prompt: Vec<String>,
    /// Emit the verdict as JSON on stdout instead of the human banner.
    pub(crate) json: bool,
}

/// Terminal state of one worker in a dispatch run. Serializes to the snake_case
/// `status` token in the JSON verdict (the wire contract), matching the
/// `#[serde(rename_all)]` convention of the payload enums in `contract.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkerStatus {
    /// Graded and passed (`--cmd` exit 0, or every `--rubric` criterion).
    Scored,
    /// Ran and was graded, but didn't pass after exhausting its retries.
    Failed,
    /// Never reached a gradeable result (boot / drive / score error).
    Errored,
}

impl WorkerStatus {
    /// Whether this terminal state counts as a pass — the single source for the
    /// verdict's `passed` field, so the two can't drift (a `Scored` worker that
    /// somehow reads `passed: false` is unrepresentable).
    pub(crate) fn passed(self) -> bool {
        matches!(self, WorkerStatus::Scored)
    }
}

/// One worker's outcome — its session, best score across retries, and how it
/// ended. Carried in input (fork) order in [`DispatchVerdict::workers`].
pub(crate) struct WorkerOutcome {
    /// The worker's session id.
    pub(crate) session: String,
    /// Best normalized score in `[0,1]` across this worker's attempts, or
    /// `None` if it never produced a gradeable result (`Errored`).
    pub(crate) score: Option<f64>,
    /// Retries this worker consumed (0 = passed/failed on the first attempt).
    pub(crate) retries_used: u32,
    /// How the worker ended. The verdict's `passed` is derived from this (see
    /// [`WorkerStatus::passed`]), not stored alongside it.
    pub(crate) status: WorkerStatus,
}

impl WorkerOutcome {
    fn to_json_value(&self) -> serde_json::Value {
        json!({
            "session": self.session,
            "score": self.score,
            "passed": self.status.passed(),
            "retries_used": self.retries_used,
            "status": self.status,
        })
    }
}

/// The verdict of a dispatch run: the selected winner, every worker's outcome,
/// and where the winner's result workspace landed. Serialized as the
/// `pillbox dispatch --json` envelope (`{version:1, dispatch:{…}}`).
pub(crate) struct DispatchVerdict {
    /// Session id of the highest-scoring worker, or `None` if none produced a
    /// gradeable result.
    pub(crate) winner: Option<String>,
    /// Every worker's outcome, in fork order.
    pub(crate) workers: Vec<WorkerOutcome>,
    /// Host directory the winner's result workspace was pulled to (`None` when
    /// there is no winner).
    pub(crate) pulled_to: Option<PathBuf>,
}

impl DispatchVerdict {
    /// The `dispatch` payload of the JSON envelope (without the `version`
    /// wrapper — [`print_json`] adds that via [`crate::paths::json_v1`]).
    fn to_json_value(&self) -> serde_json::Value {
        json!({
            "winner": self.winner,
            "workers": self.workers.iter().map(WorkerOutcome::to_json_value).collect::<Vec<_>>(),
            "pulled_to": self.pulled_to.as_ref().map(|p| p.to_string_lossy().into_owned()),
        })
    }

    /// Emit the pinned `{version:1, dispatch:{…}}` envelope on stdout.
    pub(crate) fn print_json(&self) {
        println!(
            "{}",
            crate::paths::json_v1(vec![("dispatch", self.to_json_value())])
        );
    }
}

/// Run a dispatch (stub — GHOST-002 ships the contract; GHOST-003 the loop).
///
/// Exit-code contract (`docs/dispatch.md`): a selected winner → `Ok` (0); all
/// workers failed → a plain error (1); a malformed invocation → a
/// [`PillboxError::usage`] (2).
pub(crate) fn dispatch(_resolved: &Pillbox, opts: DispatchOpts) -> Result<()> {
    // Surface validation that belongs to the contract, not the loop: `-k 0`
    // can't best-of-anything. (clap enforces the cmd-xor-rubric group itself.)
    if opts.workers < 1 {
        return Err(PillboxError::usage("dispatch", "-k must be at least 1")
            .with_next(
                "pillbox dispatch -k 3 --from-bookmark <name> --rubric <file> -- \"<prompt>\"",
            )
            .into());
    }
    bail!(
        "pillbox dispatch is not yet implemented — GHOST-002 ships the contract \
         (CLI surface + types + docs/dispatch.md); the fork/score/select loop is GHOST-003"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DispatchVerdict {
        DispatchVerdict {
            winner: Some("abc123".into()),
            workers: vec![
                WorkerOutcome {
                    session: "abc123".into(),
                    score: Some(1.0),
                    retries_used: 0,
                    status: WorkerStatus::Scored,
                },
                WorkerOutcome {
                    session: "def456".into(),
                    score: Some(0.5),
                    retries_used: 1,
                    status: WorkerStatus::Failed,
                },
                WorkerOutcome {
                    session: "ghi789".into(),
                    score: None,
                    retries_used: 0,
                    status: WorkerStatus::Errored,
                },
            ],
            pulled_to: Some(PathBuf::from("/tmp/session-abc123")),
        }
    }

    /// The `--json` envelope is the pinned downstream surface (GHOST-003/004
    /// parse it): `{version:1, dispatch:{winner, workers[], pulled_to}}`.
    #[test]
    fn verdict_json_is_the_documented_envelope() {
        let v = crate::paths::json_v1(vec![("dispatch", sample().to_json_value())]);
        let parsed: serde_json::Value = serde_json::from_str(&v).unwrap();
        assert_eq!(parsed["version"], 1);
        let d = &parsed["dispatch"];
        assert_eq!(d["winner"], "abc123");
        assert_eq!(d["pulled_to"], "/tmp/session-abc123");
        assert_eq!(d["workers"].as_array().unwrap().len(), 3);
        let w0 = &d["workers"][0];
        assert_eq!(w0["session"], "abc123");
        assert_eq!(w0["score"], 1.0);
        assert_eq!(w0["passed"], true);
        assert_eq!(w0["retries_used"], 0);
        assert_eq!(w0["status"], "scored");
    }

    /// No winner / no gradeable result serializes as JSON `null`, not absent —
    /// so a consumer can branch on it without a key-existence check.
    #[test]
    fn verdict_json_nulls_are_explicit() {
        let v = DispatchVerdict {
            winner: None,
            workers: vec![],
            pulled_to: None,
        };
        let val = v.to_json_value();
        assert!(val["winner"].is_null());
        assert!(val["pulled_to"].is_null());
        assert!(val["workers"].as_array().unwrap().is_empty());
    }

    /// The status tokens (serde rename_all) and the derived `passed` are a wire
    /// contract — pin both.
    #[test]
    fn worker_status_tokens_and_passed_are_stable() {
        assert_eq!(
            serde_json::to_value(WorkerStatus::Scored).unwrap(),
            "scored"
        );
        assert_eq!(
            serde_json::to_value(WorkerStatus::Failed).unwrap(),
            "failed"
        );
        assert_eq!(
            serde_json::to_value(WorkerStatus::Errored).unwrap(),
            "errored"
        );
        assert!(WorkerStatus::Scored.passed());
        assert!(!WorkerStatus::Failed.passed());
        assert!(!WorkerStatus::Errored.passed());
    }
}
