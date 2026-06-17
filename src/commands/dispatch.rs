//! `pillbox dispatch` — the worker-loop primitive (ghost's runtime fan-out).
//!
//! Fork `k` detached worker sessions from a snapshot bookmark, drive each to
//! idle on the same segment prompt, grade each with the rubric/cmd, retry
//! failures (feeding a distilled failure summary back as the next prompt), then
//! select the highest-scoring worker that passed and pull its result workspace.
//! Best-of-k turns the long-horizon variance σ̂ into expected gain instead of a
//! measurement enemy — which is why per-fork diversity (`--temperature`) matters:
//! `k` identical deterministic workers all score the same and select-best buys
//! nothing.
//!
//! **Orchestration is subprocess, not in-process.** The loop drives workers by
//! self-exec'ing the pillbox binary (`run`/`session …`) and parsing the
//! documented `--json` contracts — the same way the repo's existing eval /
//! router / smoke stack drives sessions (`scripts/eval/lib.sh`,
//! `scripts/router/cost-router.py`). The session handlers are private and return
//! `Result<()>`, not values, so an in-process path would mean re-shaping them +
//! the run path (a cross-cutting refactor); the CLI `--json` surface exists
//! precisely for this. The loop sits behind [`WorkerDriver`] so the selection /
//! retry **policy** is unit-tested over a mock, while the live [`CliDriver`] is
//! exercised by the GHOST-004 smoke.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::json;

use crate::contract::{Actor, Artifact, ArtifactClass, Criterion, Event, Payload, Scored};
use crate::errors::PillboxError;
use crate::events::blob::BlobStore;
use crate::events::log::SessionLog;
use crate::pillbox::Pillbox;

/// Per-turn idle timeout (seconds) each worker gets before it's treated as stuck
/// (→ an `Errored` worker, not a whole-dispatch hang). Generous — agent turns run
/// minutes. Override with `PILLBOX_DISPATCH_TURN_TIMEOUT`.
const DEFAULT_TURN_TIMEOUT_SECS: u64 = 1800;

fn turn_timeout_secs() -> u64 {
    std::env::var("PILLBOX_DISPATCH_TURN_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TURN_TIMEOUT_SECS)
}

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
    /// Per-worker retry budget when the grade fails — the loop feeds a distilled
    /// failure summary back as the next prompt and re-grades, up to this many times.
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

    /// The per-worker glyph in the human banner.
    fn as_marker(self) -> char {
        match self {
            WorkerStatus::Scored => '✓',
            WorkerStatus::Failed => '✗',
            WorkerStatus::Errored => '!',
        }
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
    /// The worker's **final** grade — the rich evidence (grader, per-criterion
    /// verdicts, feedback) the retry loop computes each turn. Retained here (not
    /// just `score`) so the dispatch-evidence artifact can preserve *why* a
    /// worker passed or failed, the substrate a later self-harness pass mines.
    /// `None` for an `Errored` worker that never produced a gradeable result.
    pub(crate) grade: Option<Scored>,
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
    /// Session id of the highest-scoring worker that passed, or `None` if none
    /// passed (the exit-1 case).
    pub(crate) winner: Option<String>,
    /// Every worker's outcome, in fork order.
    pub(crate) workers: Vec<WorkerOutcome>,
    /// Host directory the winner's result workspace was pulled to (`None` when
    /// there is no winner).
    pub(crate) pulled_to: Option<PathBuf>,
    /// Why the winner was selected, tied to the verifier output (score +
    /// tie-break) — so a reader doesn't have to re-derive the ranking from the
    /// per-worker rows. `None` when no worker passed.
    pub(crate) selection_rationale: Option<String>,
}

impl DispatchVerdict {
    /// The `dispatch` payload of the JSON envelope (without the `version`
    /// wrapper — [`print_json`] adds that via [`crate::paths::json_v1`]).
    fn to_json_value(&self) -> serde_json::Value {
        json!({
            "winner": self.winner,
            "workers": self.workers.iter().map(WorkerOutcome::to_json_value).collect::<Vec<_>>(),
            "pulled_to": self.pulled_to.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "selection_rationale": self.selection_rationale,
        })
    }

    /// Emit the pinned `{version:1, dispatch:{…}}` envelope on stdout.
    pub(crate) fn print_json(&self) {
        println!(
            "{}",
            crate::paths::json_v1(vec![("dispatch", self.to_json_value())])
        );
    }

    /// The human banner — winner + a per-worker line.
    fn print_banner(&self) {
        match &self.winner {
            Some(id) => {
                let to = self
                    .pulled_to
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                println!("pillbox: ✓ dispatch winner `{id}` → {to}");
                if let Some(why) = &self.selection_rationale {
                    println!("  selected: {why}");
                }
            }
            None => println!("pillbox: ✗ dispatch — no worker passed"),
        }
        for w in &self.workers {
            let score = w
                .score
                .map(|s| format!("{s:.2}"))
                .unwrap_or_else(|| "—".into());
            println!(
                "  {} {}  score={score} retries={}",
                w.status.as_marker(),
                w.session,
                w.retries_used
            );
        }
    }
}

/// The I/O seam the loop drives workers through. The live [`CliDriver`] shells
/// out to the pillbox binary; tests substitute a mock so the selection / retry
/// **policy** is verified without booting a VM. A worker's grade is the
/// `contract::Scored` the `session score --json` surface emits — deserialized
/// directly so a wire-contract change is a compile error, not a silent default.
trait WorkerDriver {
    /// Fork a new detached worker from the bookmark → its session id.
    fn fork(&self) -> Result<String>;
    /// Block until the worker's current turn goes idle (or terminates).
    fn wait_idle(&self, id: &str) -> Result<()>;
    /// Grade the worker's current workspace → the parsed verdict.
    fn grade(&self, id: &str) -> Result<Scored>;
    /// Drive the worker's next turn with `prompt` (the distilled retry feedback).
    fn send(&self, id: &str, prompt: &str) -> Result<()>;
    /// Pull the winner's result workspace to a durable dir → that path.
    fn pull_winner(&self, id: &str) -> Result<PathBuf>;
}

// ── pure policy (the unit-tested gate) ──────────────────────────────────────

/// The distilled failure summary fed back as the next prompt on a retry — the
/// structured signal (which checks failed + why), NOT the raw grader log, per
/// the Parallel-Distill-Refine finding (a model acts better on a distilled
/// summary than on noisy raw output). A `--rubric` grade distills to its failed
/// criteria; a `--cmd` grade falls back to the (capped) combined output.
fn distill_feedback(grade: &Scored) -> String {
    let mut out = String::from(
        "Your previous attempt did not pass verification. Address the following, then continue:\n",
    );
    let failed: Vec<&Criterion> = grade.criteria.iter().filter(|c| !c.passed).collect();
    if !failed.is_empty() {
        for c in failed {
            let why = first_lines(&c.feedback, 2);
            out.push_str(&format!("- {}: {}\n", c.name, why));
        }
    } else {
        // --cmd grade: no per-criterion detail, so distill the combined output.
        out.push_str(&first_lines(&grade.feedback, 8));
        out.push('\n');
    }
    out
}

/// Up to `n` non-blank lines of `s`, trimmed — the "distilled, not raw" cap.
fn first_lines(s: &str, n: usize) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(n)
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Index of the winning worker: the highest-scoring worker that **passed**,
/// tie-broken by fewest retries then earliest fork order. `None` when no worker
/// passed (the exit-1 case) — partial-score workers are reported in the verdict
/// but never auto-selected, so a caller can't mistake a failed attempt for a
/// success.
fn select_winner(workers: &[WorkerOutcome]) -> Option<usize> {
    workers
        .iter()
        .enumerate()
        .filter(|(_, w)| w.status.passed())
        .max_by(|(ia, a), (ib, b)| {
            // Higher score wins; then fewer retries; then earlier index. All
            // passed workers score 1.0 today, so retries/order is the real
            // discriminator — but rank by score first so the rule still holds if
            // "winner must pass" ever relaxes to "winner = best partial".
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.retries_used.cmp(&a.retries_used))
                .then(ib.cmp(ia))
        })
        .map(|(i, _)| i)
}

/// Why worker `winner` was selected — tied to the verifier output (its score,
/// against how many other passers, on what tie-break). Pure, so the rationale a
/// reader sees can't drift from the [`select_winner`] ranking it describes.
fn selection_rationale(workers: &[WorkerOutcome], winner: usize) -> String {
    let w = &workers[winner];
    let score = w
        .score
        .map(|s| format!("{s:.2}"))
        .unwrap_or_else(|| "—".into());
    let passers = workers.iter().filter(|o| o.status.passed()).count();
    if passers <= 1 {
        return format!("only passing worker (score {score})");
    }
    // >1 passer → the tie-break (fewest retries, then earliest fork) decided it.
    format!(
        "highest-ranked of {passers} passing workers: score {score}, {} retries",
        w.retries_used
    )
}

// ── dispatch evidence (#73): durable, mineable per-worker summaries ──────────

/// The structured evidence for one worker — what it was asked, how it was
/// graded, and (for the winner) why it was selected. Persisted as a
/// `dispatch.worker_summary` §0 artifact on the worker's own session log so a
/// later self-harness pass can mine *why* a worker passed or failed, not just
/// the scalar score. `Serialize` is the blob body; the §0 log keeps only the
/// small typed [`Artifact`] reference to it.
#[derive(Debug, Clone, Serialize)]
struct WorkerSummary {
    session: String,
    status: WorkerStatus,
    passed: bool,
    score: Option<f64>,
    retries_used: u32,
    /// The segment prompt this worker was driven with (turn 1).
    prompt: String,
    /// Whether this worker was the selected winner.
    winner: bool,
    /// Why it won — only on the winner (see [`selection_rationale`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    selection_rationale: Option<String>,
    /// The grader that produced the verdict (`--cmd …` / `rubric:…`). Absent for
    /// an `Errored` worker that never graded.
    #[serde(skip_serializing_if = "Option::is_none")]
    grader: Option<String>,
    /// Per-criterion verdicts from a rubric grade — the decomposed evidence
    /// (which check failed and why). Empty for a `--cmd` grade or an errored worker.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    criteria: Vec<Criterion>,
    /// The grader's combined output / rendered feedback. Absent for an errored worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    feedback: Option<String>,
    /// GHOST-011 hook: an optional, **advisory** cross-vendor judge / Fusion
    /// report (an artifact ref). Always present (null today) so the schema is
    /// forward-compatible and the slot is discoverable; the panel itself is a
    /// separate task and is never a selection input (the verifier decides).
    judge_report_ref: Option<String>,
}

impl WorkerSummary {
    fn build(o: &WorkerOutcome, prompt: &str, winner: bool, rationale: Option<&str>) -> Self {
        let (grader, criteria, feedback) = match &o.grade {
            Some(g) => (
                Some(g.grader.clone()),
                g.criteria.clone(),
                Some(g.feedback.clone()),
            ),
            None => (None, Vec::new(), None),
        };
        Self {
            session: o.session.clone(),
            status: o.status,
            passed: o.status.passed(),
            score: o.score,
            retries_used: o.retries_used,
            prompt: prompt.to_string(),
            winner,
            selection_rationale: winner.then(|| rationale.map(str::to_string)).flatten(),
            grader,
            criteria,
            feedback,
            judge_report_ref: None,
        }
    }

    /// One-line headline for the artifact's `summary` field — triage from the
    /// §0 log without dereferencing the blob.
    fn headline(&self) -> String {
        let score = self
            .score
            .map(|s| format!("{s:.2}"))
            .unwrap_or_else(|| "—".into());
        let win = if self.winner { "WINNER " } else { "" };
        format!(
            "{win}worker {} {} score {score} ({} retries)",
            self.session,
            self.status.as_marker(),
            self.retries_used
        )
    }
}

/// Persist one `dispatch.worker_summary` §0 artifact per worker — the durable,
/// mineable evidence channel (#73). Each worker's summary lands on ITS OWN
/// session log (co-located with that worker's trajectory), the body in the blob
/// store, the log line a small typed ref. Best-effort: a write failure for one
/// worker warns and is skipped, never sinking the dispatch — the run already
/// succeeded, and evidence is observability, not correctness. Stamped
/// `service:dispatch` (the orchestrator, not the worker agent).
fn record_worker_summaries(resolved: &Pillbox, verdict: &DispatchVerdict, prompt: &str) {
    for o in &verdict.workers {
        // A fork that never produced a session has nothing to attach to.
        if o.session.is_empty() {
            continue;
        }
        let winner = verdict.winner.as_deref() == Some(o.session.as_str());
        let summary =
            WorkerSummary::build(o, prompt, winner, verdict.selection_rationale.as_deref());
        if let Err(e) = write_worker_summary(resolved, &summary) {
            eprintln!(
                "pillbox: note: dispatch evidence not recorded for `{}`: {e:#}",
                o.session
            );
        }
    }
}

fn write_worker_summary(resolved: &Pillbox, summary: &WorkerSummary) -> Result<()> {
    let body = serde_json::to_vec(summary).context("serialize worker summary")?;
    let blob_ref = BlobStore::open(resolved, &summary.session)?.put(&body)?;
    let artifact = Artifact {
        kind: "dispatch.worker_summary".into(),
        summary: summary.headline(),
        content_type: "application/json".into(),
        // Carries raw grader feedback (test output / code) → content (local-only).
        class: ArtifactClass::Content,
        blob_ref,
        bytes: body.len() as u64,
        worker_id: summary.session.clone(),
    };
    SessionLog::open(resolved, &summary.session)?.append(&[Event::session(
        &summary.session,
        Payload::Artifact(artifact),
    )
    .with_actor(Actor::service("dispatch"))])?;
    Ok(())
}

// ── the loop ────────────────────────────────────────────────────────────────

/// Fork `k` workers, drive each to a terminal outcome (grade → retry → grade up
/// to `retries`), then select + pull the winner into the [`DispatchVerdict`]. A
/// worker that errors anywhere in its drive becomes an `Errored` outcome — one
/// bad worker never aborts the dispatch.
fn run_dispatch(driver: &dyn WorkerDriver, k: u32, prompt: &str, retries: u32) -> DispatchVerdict {
    // Fork all k up front (each is `--detach`, so their first turns overlap),
    // THEN drive each. A fork that fails becomes an `Errored` worker rather than
    // aborting the batch — the successes are still driven, and no forked worker is
    // left unrecorded (the orphan a `collect::<Result>()?` would leak).
    let workers: Vec<WorkerOutcome> = (0..k)
        .map(|_| driver.fork())
        .collect::<Vec<_>>()
        .into_iter()
        .map(|forked| match forked {
            Ok(id) => drive_one(driver, id, prompt, retries),
            Err(e) => {
                eprintln!("pillbox: worker fork failed: {e:#}");
                WorkerOutcome {
                    session: String::new(),
                    score: None,
                    retries_used: 0,
                    status: WorkerStatus::Errored,
                    grade: None,
                }
            }
        })
        .collect();

    let (winner, pulled_to, selection_rationale) = match select_winner(&workers) {
        Some(i) => {
            let id = workers[i].session.clone();
            let why = selection_rationale(&workers, i);
            // A pull glitch shouldn't nuke an otherwise-successful run: the winner
            // is already selected, so report it (exit 0) with a recovery hint and
            // leave `pulled_to` empty rather than propagating the error.
            let pulled = driver.pull_winner(&id).unwrap_or_else(|e| {
                eprintln!(
                    "pillbox: winner `{id}` selected but pull failed: {e:#}\n  \
                     recover with: pillbox session pull {id}"
                );
                PathBuf::new()
            });
            let pulled = (!pulled.as_os_str().is_empty()).then_some(pulled);
            (Some(id), pulled, Some(why))
        }
        None => (None, None, None),
    };
    DispatchVerdict {
        winner,
        workers,
        pulled_to,
        selection_rationale,
    }
}

/// Drive one worker to a terminal outcome. Errors are caught into an `Errored`
/// outcome (not propagated) so one worker's failure doesn't sink the others.
fn drive_one(driver: &dyn WorkerDriver, id: String, prompt: &str, retries: u32) -> WorkerOutcome {
    drive_one_inner(driver, &id, prompt, retries).unwrap_or_else(|e| {
        eprintln!("pillbox: worker `{id}` errored: {e:#}");
        WorkerOutcome {
            session: id,
            score: None,
            retries_used: 0,
            status: WorkerStatus::Errored,
            grade: None,
        }
    })
}

/// The send → grade → retry loop for one worker → its terminal outcome. Each
/// turn is driven by a `send`: turn 1 is the segment `prompt`; a failed grade
/// with budget left re-drives with the distilled failure summary. A `--detach`
/// fork comes up idle and does nothing until driven, so the first turn must be a
/// `send` (the agent's own scaffold runs it) — not a fork-baked positional,
/// which a server agent treats as a pre-fill hint, not an executed turn. Stops
/// on the first pass (`Scored`) or when the retry budget is spent (`Failed`).
fn drive_one_inner(
    driver: &dyn WorkerDriver,
    id: &str,
    prompt: &str,
    retries: u32,
) -> Result<WorkerOutcome> {
    let mut turn = prompt.to_string();
    let mut used = 0u32;
    loop {
        driver.send(id, &turn)?;
        driver.wait_idle(id)?;
        let grade = driver.grade(id)?;
        let done = grade.passed || used >= retries;
        if done {
            return Ok(WorkerOutcome {
                session: id.to_string(),
                score: Some(grade.score),
                retries_used: used,
                status: if grade.passed {
                    WorkerStatus::Scored
                } else {
                    WorkerStatus::Failed
                },
                grade: Some(grade),
            });
        }
        turn = distill_feedback(&grade);
        used += 1;
    }
}

// ── the live CLI driver (subprocess self-exec) ──────────────────────────────

/// Drives workers by self-exec'ing the pillbox binary and parsing the documented
/// `--json` contracts. All subprocesses inherit cwd, so they resolve the same
/// pillbox as the parent (the session a `fork` creates is found by a later
/// `grade`/`pull`).
struct CliDriver<'a> {
    exe: PathBuf,
    opts: &'a DispatchOpts,
    /// The `session score` grader flags (`--cmd …` xor `--rubric …`), resolved
    /// once: the clap ArgGroup guarantees exactly one, so this is never empty.
    grader: Vec<String>,
    /// Durable dir the winner is pulled into (a TempDir would drop it).
    rundir: PathBuf,
}

impl<'a> CliDriver<'a> {
    fn new(opts: &'a DispatchOpts) -> Result<Self> {
        let exe = std::env::current_exe().context("locate the pillbox binary")?;
        let grader = match (&opts.cmd, &opts.rubric) {
            (Some(c), _) => vec!["--cmd".into(), c.clone()],
            (_, Some(r)) => vec!["--rubric".into(), r.to_string_lossy().into_owned()],
            // clap's required ArgGroup makes this unreachable; a missing grader
            // would fail the `score` call loudly rather than grade nothing.
            _ => bail!("dispatch: no grader (--cmd/--rubric) — clap should have rejected this"),
        };
        let rundir = std::env::temp_dir().join(format!(
            "pillbox-dispatch-{}",
            uuid::Uuid::now_v7().simple()
        ));
        Ok(Self {
            exe,
            opts,
            grader,
            rundir,
        })
    }
}

impl WorkerDriver for CliDriver<'_> {
    fn fork(&self) -> Result<String> {
        let mut args = vec![
            "run".into(),
            "--from-bookmark".into(),
            self.opts.from_bookmark.clone(),
            "--detach".into(),
            "--json".into(),
        ];
        if let Some(a) = &self.opts.agent {
            args.extend(["--agent".into(), a.clone()]);
        }
        if let Some(m) = &self.opts.model {
            args.extend(["--model".into(), m.clone()]);
        }
        if let Some(t) = &self.opts.temperature {
            args.extend(["--temperature".into(), t.to_string()]);
        }
        if self.opts.memory {
            args.push("--memory".into());
        }
        // No positional prompt: a `--detach` worker comes up idle; the segment
        // prompt is driven as turn 1 via `session send` (see `drive_one_inner`).
        let out = self.capture(&args)?;
        let v: serde_json::Value = serde_json::from_str(&out)
            .with_context(|| format!("parse `run --json` output: {out:?}"))?;
        v["session"]["id"]
            .as_str()
            .map(str::to_string)
            .context("`run --json` had no session.id")
    }

    fn wait_idle(&self, id: &str) -> Result<()> {
        // Bounded so one stuck worker becomes an Errored worker (the caller maps
        // this Err → Errored), not a whole-dispatch hang. `wait-idle` exits 0 on
        // idle/terminated, 1 on timeout — so any non-zero here is a stuck turn.
        self.status(&[
            "session".into(),
            "wait-idle".into(),
            id.into(),
            "--timeout".into(),
            turn_timeout_secs().to_string(),
        ])
    }

    fn grade(&self, id: &str) -> Result<Scored> {
        // Grade the worker's *live* workspace clone in place (no pull): `session
        // info --json` exposes its path, `session score --workspace` grades it.
        // The winner is pulled to a durable dir separately (`pull_winner`).
        // NOTE: `.session.workspace` resolves via the session's `LiveSession`
        // `workspace_path()` — libkrun sessions only (docker has no result clone).
        // Docker dispatch needs a different workspace resolution (see
        // docs/dispatch.md Deferred); v1 is libkrun-only.
        let info = self.capture(&["session".into(), "info".into(), id.into(), "--json".into()])?;
        let iv: serde_json::Value = serde_json::from_str(&info)
            .with_context(|| format!("parse `session info --json`: {info:?}"))?;
        let ws = iv["session"]["workspace"]
            .as_str()
            .context("`session info --json` had no session.workspace (libkrun-only today)")?
            .to_string();

        let mut args = vec![
            "session".into(),
            "score".into(),
            id.into(),
            "--workspace".into(),
            ws,
            "--json".into(),
        ];
        args.extend(self.grader.iter().cloned());
        let out = self.capture(&args)?;
        parse_grade(&out)
    }

    fn send(&self, id: &str, prompt: &str) -> Result<()> {
        self.status(&["session".into(), "send".into(), id.into(), prompt.into()])
    }

    fn pull_winner(&self, id: &str) -> Result<PathBuf> {
        let to = self.rundir.join(format!("winner-{id}"));
        std::fs::create_dir_all(&self.rundir)
            .with_context(|| format!("create {:?}", self.rundir))?;
        self.status(&[
            "session".into(),
            "pull".into(),
            id.into(),
            "--to".into(),
            to.to_string_lossy().into_owned(),
        ])?;
        Ok(to)
    }
}

impl CliDriver<'_> {
    /// Run a pillbox subcommand, require success, return stdout.
    fn capture(&self, args: &[String]) -> Result<String> {
        let out = Command::new(&self.exe)
            .args(args)
            .output()
            .with_context(|| format!("spawn `pillbox {}`", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "`pillbox {}` failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a pillbox subcommand for its exit status only (success = Ok). Its
    /// stdout — a per-command human banner (`sent prompt…`, `…idle`, `✓
    /// restored…`) — is captured and **discarded**, never inherited: dispatch's
    /// own stdout must stay pure JSON for `--json`. stderr surfaces on failure.
    fn status(&self, args: &[String]) -> Result<()> {
        let out = Command::new(&self.exe)
            .args(args)
            .output()
            .with_context(|| format!("spawn `pillbox {}`", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "`pillbox {}` failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Parse a `session score --json` envelope into the `contract::Scored` it
/// serializes (the extra `version`/`session`/`seq` envelope keys are ignored).
fn parse_grade(out: &str) -> Result<Scored> {
    serde_json::from_str(out).with_context(|| format!("parse `score --json`: {out:?}"))
}

// ── handler ─────────────────────────────────────────────────────────────────

/// Run a dispatch. Exit-code contract (`docs/dispatch.md`): a selected winner →
/// `Ok` (0); no worker passed → a plain error (1); a malformed invocation → a
/// [`PillboxError::usage`] (2). The `--json` verdict is printed on **both** the
/// winner and no-winner paths (so a caller always reads every worker's score);
/// the no-winner exit-1 error then rides stderr.
pub(crate) fn dispatch(resolved: &Pillbox, opts: DispatchOpts) -> Result<()> {
    if opts.workers < 1 {
        return Err(PillboxError::usage("dispatch", "-k must be at least 1")
            .with_next(
                "pillbox dispatch -k 3 --from-bookmark <name> --rubric <file> -- \"<prompt>\"",
            )
            .into());
    }
    if opts.prompt.iter().all(|p| p.trim().is_empty()) {
        return Err(PillboxError::usage(
            "dispatch",
            "no segment prompt — workers have nothing to do",
        )
        .with_next("pillbox dispatch … -- \"<the segment prompt>\"")
        .into());
    }

    let driver = CliDriver::new(&opts)?;
    let prompt = opts.prompt.join(" ");
    let verdict = run_dispatch(&driver, opts.workers, &prompt, opts.retries);

    // Persist each worker's evidence to its §0 log (#73) before reporting —
    // best-effort, so a log-write hiccup never changes the dispatch outcome.
    record_worker_summaries(resolved, &verdict, &prompt);

    if opts.json {
        verdict.print_json();
    } else {
        verdict.print_banner();
    }
    if verdict.winner.is_none() {
        // Verdict already printed (every worker's score is on stdout); signal the
        // no-winner outcome with exit 1.
        bail!("dispatch: no worker passed verification");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn outcome(
        session: &str,
        score: Option<f64>,
        retries: u32,
        status: WorkerStatus,
    ) -> WorkerOutcome {
        WorkerOutcome {
            session: session.into(),
            score,
            retries_used: retries,
            status,
            grade: None,
        }
    }

    // ── selection policy ──

    #[test]
    fn select_winner_picks_highest_passing_score() {
        let ws = vec![
            outcome("a", Some(1.0), 0, WorkerStatus::Scored),
            outcome("b", Some(0.9), 0, WorkerStatus::Failed), // higher-than-some but didn't pass
            outcome("c", Some(1.0), 0, WorkerStatus::Scored),
        ];
        // a and c both passed at 1.0; tie-break → earliest (a).
        assert_eq!(select_winner(&ws), Some(0));
    }

    #[test]
    fn select_winner_tiebreaks_fewer_retries_then_earliest() {
        let ws = vec![
            outcome("a", Some(1.0), 2, WorkerStatus::Scored),
            outcome("b", Some(1.0), 0, WorkerStatus::Scored), // fewer retries wins
            outcome("c", Some(1.0), 0, WorkerStatus::Scored),
        ];
        assert_eq!(select_winner(&ws), Some(1));
    }

    #[test]
    fn select_winner_none_when_no_worker_passed() {
        let ws = vec![
            outcome("a", Some(0.8), 1, WorkerStatus::Failed),
            outcome("b", None, 0, WorkerStatus::Errored),
        ];
        // A high partial score is reported but never auto-selected.
        assert_eq!(select_winner(&ws), None);
    }

    // ── retry feedback distillation ──

    fn scored(passed: bool, score: f64, criteria: Vec<Criterion>, feedback: &str) -> Scored {
        Scored {
            grader: "test".into(),
            passed,
            score,
            feedback: feedback.into(),
            criteria,
        }
    }

    fn criterion(name: &str, passed: bool, feedback: &str) -> Criterion {
        Criterion {
            name: name.into(),
            passed,
            feedback: feedback.into(),
        }
    }

    #[test]
    fn distill_feedback_lists_only_failed_criteria() {
        let g = scored(
            false,
            0.5,
            vec![
                criterion("compiles", true, "ok"),
                criterion("tests", false, "2 failed\nassertion error"),
            ],
            "",
        );
        let fb = distill_feedback(&g);
        assert!(fb.contains("tests: 2 failed / assertion error"));
        assert!(
            !fb.contains("compiles"),
            "passed criteria must not be echoed"
        );
    }

    #[test]
    fn distill_feedback_falls_back_to_capped_output_for_cmd_grade() {
        let g = scored(false, 0.0, vec![], "line1\n\nline2\nline3");
        let fb = distill_feedback(&g);
        assert!(fb.contains("line1 / line2 / line3"));
    }

    // ── the loop over a mock driver ──

    /// A scripted driver: each fork hands out the next id; `grade` replays a
    /// per-id queue of (passed, score) so a test can model retry-then-pass.
    struct MockDriver {
        ids: RefCell<std::collections::VecDeque<String>>,
        grades: RefCell<std::collections::HashMap<String, std::collections::VecDeque<(bool, f64)>>>,
        /// Worker ids whose `grade` raises an error (models a stuck/broken worker).
        grade_errs: std::collections::HashSet<String>,
        sends: RefCell<usize>,
        pulls: RefCell<Vec<String>>,
    }

    impl MockDriver {
        fn new(grade_script: Vec<(&str, Vec<(bool, f64)>)>) -> Self {
            let ids = grade_script.iter().map(|(id, _)| id.to_string()).collect();
            let grades = grade_script
                .into_iter()
                .map(|(id, q)| (id.to_string(), q.into_iter().collect()))
                .collect();
            Self {
                ids: RefCell::new(ids),
                grades: RefCell::new(grades),
                grade_errs: std::collections::HashSet::new(),
                sends: RefCell::new(0),
                pulls: RefCell::new(Vec::new()),
            }
        }
        fn failing_grade(mut self, id: &str) -> Self {
            self.grade_errs.insert(id.to_string());
            self
        }
    }

    impl WorkerDriver for MockDriver {
        fn fork(&self) -> Result<String> {
            Ok(self.ids.borrow_mut().pop_front().expect("fork over budget"))
        }
        fn wait_idle(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        fn grade(&self, id: &str) -> Result<Scored> {
            if self.grade_errs.contains(id) {
                bail!("mock: grade failed for {id}");
            }
            let (passed, score) = self
                .grades
                .borrow_mut()
                .get_mut(id)
                .and_then(|q| q.pop_front())
                .expect("grade over budget");
            Ok(scored(passed, score, vec![], ""))
        }
        fn send(&self, _id: &str, _prompt: &str) -> Result<()> {
            *self.sends.borrow_mut() += 1;
            Ok(())
        }
        fn pull_winner(&self, id: &str) -> Result<PathBuf> {
            self.pulls.borrow_mut().push(id.to_string());
            Ok(PathBuf::from(format!("/tmp/winner-{id}")))
        }
    }

    #[test]
    fn loop_selects_winner_and_pulls_it() {
        // worker "w0" fails then passes (1 retry); "w1" passes first try.
        let d = MockDriver::new(vec![
            ("w0", vec![(false, 0.5), (true, 1.0)]),
            ("w1", vec![(true, 1.0)]),
        ]);
        let v = run_dispatch(&d, 2, "task", 2);
        assert_eq!(v.workers.len(), 2);
        assert_eq!(v.workers[0].retries_used, 1);
        assert_eq!(v.workers[1].retries_used, 0);
        // Both passed at 1.0 → tie-break on retries → w1 (0 retries).
        assert_eq!(v.winner.as_deref(), Some("w1"));
        assert_eq!(v.pulled_to, Some(PathBuf::from("/tmp/winner-w1")));
        // 3 sends: each worker's turn-1 prompt (×2) + w0's one retry.
        assert_eq!(*d.sends.borrow(), 3);
        assert_eq!(
            *d.pulls.borrow(),
            vec!["w1".to_string()],
            "only the winner is pulled"
        );
    }

    #[test]
    fn loop_isolates_an_errored_worker_and_still_picks_a_winner() {
        // "bad" errors during grade; "good" passes. The errored worker must not
        // sink the batch or win — the good one is selected.
        let d = MockDriver::new(vec![("bad", vec![]), ("good", vec![(true, 1.0)])])
            .failing_grade("bad");
        let v = run_dispatch(&d, 2, "task", 0);
        assert_eq!(v.workers[0].status, WorkerStatus::Errored);
        assert_eq!(v.workers[0].score, None);
        assert_eq!(v.winner.as_deref(), Some("good"));
        assert_eq!(*d.pulls.borrow(), vec!["good".to_string()]);
    }

    #[test]
    fn loop_respects_retry_budget_then_fails() {
        // never passes; budget 1 → 1 retry then Failed, no winner, no pull.
        let d = MockDriver::new(vec![("w0", vec![(false, 0.3), (false, 0.4)])]);
        let v = run_dispatch(&d, 1, "task", 1);
        assert_eq!(v.workers[0].status, WorkerStatus::Failed);
        assert_eq!(v.workers[0].retries_used, 1);
        assert_eq!(v.workers[0].score, Some(0.4));
        assert_eq!(v.winner, None);
        assert_eq!(v.pulled_to, None);
        // 2 sends: the turn-1 prompt + one retry.
        assert_eq!(*d.sends.borrow(), 2);
        assert!(d.pulls.borrow().is_empty());
    }

    // ── verdict JSON shape (the downstream wire contract) ──

    fn sample() -> DispatchVerdict {
        DispatchVerdict {
            winner: Some("abc123".into()),
            workers: vec![
                outcome("abc123", Some(1.0), 0, WorkerStatus::Scored),
                outcome("def456", Some(0.5), 1, WorkerStatus::Failed),
                outcome("ghi789", None, 0, WorkerStatus::Errored),
            ],
            pulled_to: Some(PathBuf::from("/tmp/session-abc123")),
            selection_rationale: Some("only passing worker (score 1.00)".into()),
        }
    }

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

    #[test]
    fn verdict_json_nulls_are_explicit() {
        let v = DispatchVerdict {
            winner: None,
            workers: vec![],
            pulled_to: None,
            selection_rationale: None,
        };
        let val = v.to_json_value();
        assert!(val["winner"].is_null());
        assert!(val["pulled_to"].is_null());
        assert!(val["selection_rationale"].is_null());
        assert!(val["workers"].as_array().unwrap().is_empty());
    }

    // ── dispatch evidence (#73) ──

    #[test]
    fn selection_rationale_distinguishes_sole_vs_tiebroken_winner() {
        // Sole passer → "only passing worker".
        let sole = vec![
            outcome("a", Some(1.0), 0, WorkerStatus::Scored),
            outcome("b", Some(0.5), 1, WorkerStatus::Failed),
        ];
        assert!(selection_rationale(&sole, 0).starts_with("only passing worker"));
        // >1 passer → names the field size + the tie-break inputs.
        let many = vec![
            outcome("a", Some(1.0), 0, WorkerStatus::Scored),
            outcome("b", Some(1.0), 2, WorkerStatus::Scored),
        ];
        let why = selection_rationale(&many, 0);
        assert!(why.contains("2 passing workers"), "{why}");
        assert!(why.contains("0 retries"), "{why}");
    }

    #[test]
    fn worker_summary_captures_grade_evidence_and_judge_hook() {
        let mut o = outcome("w1", Some(1.0), 1, WorkerStatus::Scored);
        o.grade = Some(scored(
            true,
            1.0,
            vec![
                criterion("tests", true, "5 passed"),
                criterion("lint", true, ""),
            ],
            "all green",
        ));
        let s = WorkerSummary::build(&o, "implement add()", true, Some("only passing worker"));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["session"], "w1");
        assert_eq!(v["status"], "scored");
        assert_eq!(v["passed"], true);
        assert_eq!(v["prompt"], "implement add()");
        assert_eq!(v["winner"], true);
        assert_eq!(v["selection_rationale"], "only passing worker");
        assert_eq!(v["grader"], "test");
        assert_eq!(v["criteria"].as_array().unwrap().len(), 2);
        assert_eq!(v["feedback"], "all green");
        // The GHOST-011 judge slot is present-and-null (forward-compatible), not omitted.
        assert!(v.get("judge_report_ref").is_some() && v["judge_report_ref"].is_null());
    }

    #[test]
    fn worker_summary_for_errored_worker_omits_grade_and_rationale() {
        let o = outcome("bad", None, 0, WorkerStatus::Errored);
        let s = WorkerSummary::build(&o, "task", false, Some("n/a"));
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["status"], "errored");
        assert_eq!(v["passed"], false);
        // No grade → grader/criteria/feedback omitted; non-winner → no rationale.
        assert!(v.get("grader").is_none());
        assert!(v.get("criteria").is_none());
        assert!(v.get("feedback").is_none());
        assert!(v.get("selection_rationale").is_none());
    }

    #[test]
    fn record_worker_summaries_writes_an_artifact_to_each_worker_log() {
        crate::test_util::with_isolated_home("dispatch-evidence-roundtrip", || {
            use crate::contract::Payload;
            let pb = crate::pillbox::global();
            // The worker's session must resolve for BlobStore/SessionLog::open.
            let s = crate::session::Session::test_fixture(); // id = abc123def456
            crate::session::write(&pb, &s).unwrap();

            let mut w = outcome(&s.id, Some(1.0), 1, WorkerStatus::Scored);
            w.grade = Some(scored(
                true,
                1.0,
                vec![criterion("tests", true, "ok")],
                "green",
            ));
            let verdict = DispatchVerdict {
                winner: Some(s.id.clone()),
                workers: vec![w],
                pulled_to: None,
                selection_rationale: Some("only passing worker (score 1.00)".into()),
            };

            record_worker_summaries(&pb, &verdict, "implement add()");

            // The worker's §0 log now carries one dispatch.worker_summary artifact...
            let events = crate::events::log::SessionLog::open(&pb, &s.id)
                .unwrap()
                .read_from(0)
                .unwrap();
            let ev = events
                .iter()
                .find(|e| matches!(e.payload, Payload::Artifact(_)))
                .expect("worker-summary artifact on the worker's log");
            let Payload::Artifact(art) = &ev.payload else {
                unreachable!()
            };
            assert_eq!(art.kind, "dispatch.worker_summary");
            assert_eq!(art.worker_id, s.id);
            assert_eq!(ev.actor.as_ref().unwrap().id, "svc:dispatch");

            // ...and the blob round-trips to the full WorkerSummary evidence.
            let body = crate::events::blob::BlobStore::open(&pb, &s.id)
                .unwrap()
                .get(&art.blob_ref)
                .unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(parsed["winner"], true);
            assert_eq!(
                parsed["selection_rationale"],
                "only passing worker (score 1.00)"
            );
            assert_eq!(parsed["prompt"], "implement add()");
            assert_eq!(parsed["criteria"][0]["name"], "tests");
        });
    }

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
