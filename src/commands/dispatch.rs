//! `pillbox dispatch` — the worker-loop primitive (ghost's runtime fan-out).
//!
//! Fork `k` detached worker sessions from a snapshot bookmark, drive each to a
//! terminal outcome, grade each with the rubric/cmd reward, select the
//! highest-scoring worker that passed, and pull its result workspace. Each worker
//! runs in one of two modes:
//!
//! - **fork-`k` (default):** every worker gets the same single prompt + retry —
//!   the **best-of-k diversity** axis. Per-fork diversity (`--temperature`)
//!   matters: `k` identical deterministic workers all score the same and
//!   select-best buys nothing.
//! - **`--segments` (the proven segmentation lever):** each worker drives an
//!   ordered chain of focused, checkpoint-gated sub-prompts in ONE session
//!   (`drive_segments_inner`) — context accumulates, the horizon never resets.
//!   The σ̂ experiments (`docs/optimization-gate.md`) showed this, not
//!   fork-per-segment, is what cuts variance + lifts the mean. The two compose:
//!   `--segments -k N` = best-of-k over segmented chains.
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
// Context: doc://pillbox/adr-008-ghost-extraction-trigger@0001#ghost-extraction-trigger

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::json;

use crate::commands::critic::{
    edits_from_log, render_state, Critic, CriticPolicy, CriticVerdict, EditRecord, TypesafeCritic,
};
use crate::contract::{Actor, Artifact, ArtifactClass, Criterion, Event, Payload, Scored};
use crate::errors::PillboxError;
use crate::events::blob::BlobStore;
use crate::events::log::SessionLog;
use crate::pillbox::Pillbox;

/// Per-turn idle timeout (seconds) each worker gets before it's treated as stuck
/// (→ an `Errored` worker, not a whole-dispatch hang). Generous — agent turns run
/// minutes. Override with `PILLBOX_DISPATCH_TURN_TIMEOUT`.
const DEFAULT_TURN_TIMEOUT_SECS: u64 = 1800;

/// The clap default for `-k`/`--workers` (mirrors `main.rs`'s `default_value_t`).
/// Used to tell an explicit `-k N` from the default when a `--workers-spec` roster
/// supplies the count: a roster present alongside an explicit, *disagreeing* `-k`
/// is a usage error, but a roster with no explicit `-k` (the value is still the
/// default) just derives `k` from the roster length.
const DEFAULT_WORKERS: u32 = 3;

fn turn_timeout_secs() -> u64 {
    std::env::var("PILLBOX_DISPATCH_TURN_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TURN_TIMEOUT_SECS)
}

/// A grader spec — the `--cmd X` xor `--rubric FILE` pair `session score`
/// accepts. Used for both the run-level **reward** (the authoritative final
/// grade) and each segment **gate**, so one `grade(id, grader)` seam covers both.
#[derive(Debug, Clone)]
enum Grader {
    Cmd(String),
    Rubric(PathBuf),
}

impl Grader {
    /// The run-level reward grader from the dispatch opts. clap's required
    /// ArgGroup guarantees exactly one of `cmd`/`rubric`, so the `_` arm is
    /// unreachable in practice — kept as a loud usage error, not a panic.
    fn from_opts(opts: &DispatchOpts) -> Result<Self> {
        match (&opts.cmd, &opts.rubric) {
            (Some(c), _) => Ok(Grader::Cmd(c.clone())),
            (_, Some(r)) => Ok(Grader::Rubric(r.clone())),
            _ => Err(
                PillboxError::usage("dispatch", "no grader (--cmd/--rubric)")
                    .with_next("pass --rubric <file> or --cmd \"<verifier>\"")
                    .into(),
            ),
        }
    }

    /// The `session score` flags for this grader.
    fn flags(&self) -> Vec<String> {
        match self {
            Grader::Cmd(c) => vec!["--cmd".into(), c.clone()],
            Grader::Rubric(p) => vec!["--rubric".into(), p.to_string_lossy().into_owned()],
        }
    }
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
    /// `--workers-spec FILE`: a heterogeneous worker roster (parsed + validated).
    /// When `Some`, each `[[worker]]` row binds the `i`-th fork's agent/model/
    /// temperature, falling back per field to the scalar `agent`/`model`/
    /// `temperature` above (themselves falling back to `pillbox.toml`); `k` is
    /// derived from the roster length. `None` → today's homogeneous fork-`k` (the
    /// scalar opts apply to every worker), byte-identical to before.
    pub(crate) workers_spec: Option<Vec<WorkerSpec>>,
    /// Wire in kypp swarm-memory (`--memory`) for each worker.
    pub(crate) memory: bool,
    /// Per-worker retention TTL (`30m`/`24h`/`7d`), forwarded to each worker's
    /// `run --detach --ttl`. Losers are left running (not auto-killed), so a TTL
    /// is how a dispatch campaign reaps them later via `session prune` without
    /// orphaning their §0 evidence (`session rm` removes the record the evidence
    /// is read through). `None` → workers persist until manual `session rm`.
    pub(crate) ttl: Option<String>,
    /// `--segments SPEC`: drive an ordered segment chain (TOML) in ONE session per
    /// worker — the proven in-session segmentation lever — instead of one prompt.
    /// `None` → the fork-`k`-on-one-prompt path. The run-level `cmd`/`rubric` stays
    /// the final reward; each segment carries its own gate.
    pub(crate) segments: Option<PathBuf>,
    /// The task prompt handed to every worker (the positional `-- args`). In
    /// fork-`k` mode this is the work (required); in `--segments` mode the segments
    /// carry the work and this is optional context prepended to segment 1.
    pub(crate) prompt: Vec<String>,
    /// Emit the verdict as JSON on stdout instead of the human banner.
    pub(crate) json: bool,
    /// `--critic KIND`: score each worker's edits with a calibrated System One
    /// critic before the verifier runs (`typesafe` today). `None` → no critic,
    /// byte-identical output to before.
    pub(crate) critic: Option<String>,
    /// `--critic-policy`: `record` (measure only) | `order` (grade in descending
    /// p, stop at the first passer) | `select` (grade only the argmax-p worker).
    pub(crate) critic_policy: String,
    /// `--critic-model`: the critic's model id (`jev-latest`).
    pub(crate) critic_model: String,
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
    /// Reached idle but was never graded: a `--critic-policy order` run found a
    /// passer earlier in critic order, or `select` picked another worker. Its
    /// `critic.p` is the only signal about it. Never a pass, never a winner.
    Unverified,
}

impl WorkerStatus {
    /// Whether this terminal state counts as a pass — the single source for the
    /// verdict's `passed` field, so the two can't drift (a `Scored` worker that
    /// somehow reads `passed: false` is unrepresentable).
    pub(crate) fn passed(self) -> bool {
        matches!(self, WorkerStatus::Scored)
    }

    /// Whether the verifier ran on this worker at all (`Unverified`/`Errored`
    /// never cost a verifier run).
    fn verified(self) -> bool {
        matches!(self, WorkerStatus::Scored | WorkerStatus::Failed)
    }

    /// The per-worker glyph in the human banner.
    fn as_marker(self) -> char {
        match self {
            WorkerStatus::Scored => '✓',
            WorkerStatus::Failed => '✗',
            WorkerStatus::Errored => '!',
            WorkerStatus::Unverified => '?',
        }
    }
}

/// One segment's outcome in a `--segments` chain — its gate verdict, score, and
/// retries. The per-checkpoint trajectory, surfaced in the verdict (additive) and
/// the §0 evidence. A failed gate does not abort the chain (the final reward is
/// authoritative), so a `passed: false` segment can still precede a winning worker.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SegmentOutcome {
    pub(crate) name: String,
    pub(crate) passed: bool,
    pub(crate) score: f64,
    pub(crate) retries_used: u32,
}

/// One worker's outcome — its session, best score across retries, and how it
/// ended. Carried in input (fork) order in [`DispatchVerdict::workers`].
pub(crate) struct WorkerOutcome {
    /// The worker's session id.
    pub(crate) session: String,
    /// Best normalized score in `[0,1]` across this worker's attempts, or
    /// `None` if it never produced a gradeable result (`Errored`).
    pub(crate) score: Option<f64>,
    /// Retries this worker consumed (0 = passed/failed on the first attempt). In
    /// `--segments` mode this is the SUM of per-segment retries.
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
    /// Per-segment outcomes in a `--segments` run, in order. Empty for a fork-`k`
    /// (single-prompt) worker or an errored one — so the verdict JSON omits
    /// `segments` there. A non-errored `--segments` worker always has ≥1 (an empty
    /// spec is rejected at parse), so empty-vs-non-empty is a sound mode discriminant.
    pub(crate) segments: Vec<SegmentOutcome>,
    /// The critic's probability for this worker, taken right before its final
    /// grade (`--critic`). `None` without a critic, or when the critic call
    /// failed (a critic failure never sinks a worker — it just goes unscored).
    pub(crate) critic: Option<CriticVerdict>,
}

impl WorkerOutcome {
    fn to_json_value(&self) -> serde_json::Value {
        let mut v = json!({
            "session": self.session,
            "score": self.score,
            "passed": self.status.passed(),
            "retries_used": self.retries_used,
            "status": self.status,
        });
        // Additive: only present for a `--segments` worker, so fork-`k` output is
        // byte-identical to before.
        if !self.segments.is_empty() {
            v["segments"] = serde_json::to_value(&self.segments).unwrap_or_else(|_| json!([]));
        }
        // Additive: only present when a `--critic` scored this worker.
        if let Some(c) = &self.critic {
            v["critic"] = serde_json::to_value(c).unwrap_or(serde_json::Value::Null);
        }
        v
    }

    /// Verifier runs this worker cost: one grade per attempt, none if it was
    /// never graded.
    fn verifier_runs(&self) -> u32 {
        if self.status.verified() {
            1 + self.retries_used
        } else {
            0
        }
    }
}

/// The critic block of the verdict — present only when `--critic` was given.
/// `verifier_runs_saved` is the count of workers left `unverified` (each would
/// have cost at least one grade), the number the `order`/`select` policies exist
/// to move.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CriticRun {
    pub(crate) kind: String,
    pub(crate) policy: CriticPolicy,
    pub(crate) model: String,
    pub(crate) verifier_runs: u32,
    pub(crate) verifier_runs_saved: u32,
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
    /// The critic that ran and what it saved — `None` without `--critic`, so the
    /// envelope is byte-identical to before.
    pub(crate) critic: Option<CriticRun>,
}

impl DispatchVerdict {
    /// The `dispatch` payload of the JSON envelope (without the `version`
    /// wrapper — [`print_json`] adds that via [`crate::paths::json_v1`]).
    fn to_json_value(&self) -> serde_json::Value {
        let mut v = json!({
            "winner": self.winner,
            "workers": self.workers.iter().map(WorkerOutcome::to_json_value).collect::<Vec<_>>(),
            "pulled_to": self.pulled_to.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "selection_rationale": self.selection_rationale,
        });
        // Additive: only present when a `--critic` ran.
        if let Some(c) = &self.critic {
            v["critic"] = serde_json::to_value(c).unwrap_or(serde_json::Value::Null);
        }
        v
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
        if let Some(c) = &self.critic {
            println!(
                "  critic: {} ({}) policy={} verifier_runs={} saved={}",
                c.kind,
                c.model,
                c.policy.as_str(),
                c.verifier_runs,
                c.verifier_runs_saved
            );
        }
        for w in &self.workers {
            let score = w
                .score
                .map(|s| format!("{s:.2}"))
                .unwrap_or_else(|| "—".into());
            let critic = w
                .critic
                .as_ref()
                .map(|c| format!(" critic_p={:.2}", c.p))
                .unwrap_or_default();
            println!(
                "  {} {}  score={score} retries={}{critic}",
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
    /// Fork a new detached worker (the `i`-th, 0-based) from the bookmark → its
    /// session id. The index picks this worker's roster row (`--workers-spec`);
    /// without a roster it's ignored and every fork is identical.
    fn fork(&self, i: usize, first_turn: &str) -> Result<String>;
    /// True when this worker's first turn is consumed by the launch argv instead
    /// of the in-session prompt API / PTY send path.
    fn first_turn_driven_on_fork(&self, i: usize) -> bool;
    /// Block until the worker's current turn goes idle (or terminates).
    fn wait_idle(&self, id: &str) -> Result<()>;
    /// Grade the worker's current workspace against `grader` → the parsed verdict.
    /// One seam for both the run-level reward and per-segment gates.
    fn grade(&self, id: &str, grader: &Grader) -> Result<Scored>;
    /// Drive the worker's next turn with `prompt` (the distilled retry feedback).
    fn send(&self, id: &str, prompt: &str) -> Result<()>;
    /// Pull the winner's result workspace to a durable dir → that path.
    fn pull_winner(&self, id: &str) -> Result<PathBuf>;
    /// The worker's edits so far, from its §0 log — what a `--critic` judges.
    fn edits(&self, id: &str) -> Result<Vec<EditRecord>>;
}

/// The critic, threaded through the drive paths as one handle. `None` = no
/// `--critic` — every path below is byte-for-byte the pre-critic loop then.
struct CriticCtx<'a> {
    critic: &'a dyn Critic,
    /// The task prompt the worker was driven with — half of the critic's state.
    prompt: &'a str,
}

impl CriticCtx<'_> {
    /// Score one worker's current edits. Errors are logged and swallowed into
    /// `None`: the critic is advisory, and an HTTP hiccup must not turn a
    /// gradeable worker into an `Errored` one.
    fn score(&self, driver: &dyn WorkerDriver, id: &str) -> Option<CriticVerdict> {
        let edits = match driver.edits(id) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("pillbox: critic skipped `{id}` (edits unreadable): {e:#}");
                return None;
            }
        };
        let (state, _truncated) = render_state(self.prompt, &edits);
        match self.critic.score(&state) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("pillbox: critic failed for `{id}`: {e:#}");
                None
            }
        }
    }
}

// ── pure policy (the unit-tested gate) ──────────────────────────────────────

/// Resolve the `i`-th worker's `(agent, model, temperature)` from the opts —
/// pure, so the `--workers-spec` → run-level fallback precedence is unit-testable
/// without a VM. With a roster, this worker's row wins per field and the
/// run-level scalar opt is the fallback; without one, every worker gets the
/// scalar opts (today's homogeneous fork-`k`). The handler sets `k =
/// roster.len()`, so an in-bounds `i` always indexes a row when a roster exists.
fn resolve_worker(opts: &DispatchOpts, i: usize) -> (Option<String>, Option<String>, Option<f64>) {
    match opts.workers_spec.as_ref().and_then(|r| r.get(i)) {
        Some(w) => (
            w.agent.clone().or_else(|| opts.agent.clone()),
            w.model.clone().or_else(|| opts.model.clone()),
            w.temperature.or(opts.temperature),
        ),
        None => (opts.agent.clone(), opts.model.clone(), opts.temperature),
    }
}

fn effective_worker_agent(opts: &DispatchOpts, i: usize, default_agent: &str) -> String {
    resolve_worker(opts, i)
        .0
        .unwrap_or_else(|| default_agent.to_string())
}

fn default_agent(resolved: &Pillbox) -> String {
    crate::config::resolve_run_config(resolved)
        .agent
        .unwrap_or_else(|| "claude".into())
}

fn agent_first_turn_driven_on_fork(agent: &str) -> bool {
    crate::agents::lookup("dispatch", agent)
        .map(|spec| spec.server.is_none())
        .unwrap_or(false)
}

fn worker_first_turn_driven_on_fork(opts: &DispatchOpts, i: usize, default_agent: &str) -> bool {
    let agent = effective_worker_agent(opts, i, default_agent);
    agent_first_turn_driven_on_fork(&agent)
}

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
    // A critic's p rides along so a reader can see whether the critic would
    // have picked the same worker — it never decides; the verifier did.
    let critic = w
        .critic
        .as_ref()
        .map(|c| format!("; critic p={:.2}", c.p))
        .unwrap_or_default();
    if passers <= 1 {
        return format!("only passing worker (score {score}{critic})");
    }
    // >1 passer → the tie-break (fewest retries, then earliest fork) decided it.
    format!(
        "highest-ranked of {passers} passing workers: score {score}, {} retries{critic}",
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
    /// Per-segment outcomes for a `--segments` worker — the checkpoint trajectory a
    /// later self-harness pass mines. Empty (omitted) for a fork-`k` worker.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    segments: Vec<SegmentOutcome>,
    /// GHOST-011 hook: an optional, **advisory** cross-vendor judge / Fusion
    /// report (an artifact ref). Always present (null today) so the schema is
    /// forward-compatible and the slot is discoverable; the panel itself is a
    /// separate task and is never a selection input (the verifier decides).
    judge_report_ref: Option<String>,
    /// The `--critic` probability recorded right before this worker's final
    /// grade — the `(p, passed)` pair the harness learns its critic from. Omitted
    /// when no critic ran. Also written on its own as a `dispatch.critic_verdict`
    /// signal artifact so it can pool without this content-class body.
    #[serde(skip_serializing_if = "Option::is_none")]
    critic: Option<CriticVerdict>,
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
            segments: o.segments.clone(),
            judge_report_ref: None,
            critic: o.critic.clone(),
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
        // The critic's verdict also lands on its own, as poolable signal: p next to
        // the verifier's passed/score, no prompt, no code. This is the labeled pair
        // every dispatch contributes to measuring the critic.
        if let (Some(c), Some(run)) = (&o.critic, &verdict.critic) {
            if let Err(e) = write_critic_verdict(resolved, o, c, run) {
                eprintln!(
                    "pillbox: note: critic verdict not recorded for `{}`: {e:#}",
                    o.session
                );
            }
        }
    }
}

/// Persist one `dispatch.critic_verdict` §0 artifact (signal class) on the
/// worker's own log: the critic's `p` alongside the verifier's ground truth.
fn write_critic_verdict(
    resolved: &Pillbox,
    o: &WorkerOutcome,
    c: &CriticVerdict,
    run: &CriticRun,
) -> Result<()> {
    let record = json!({
        "session": o.session,
        "critic": c,
        "policy": run.policy,
        "status": o.status,
        "verified": o.status.verified(),
        "passed": o.status.passed(),
        "score": o.score,
        "retries_used": o.retries_used,
    });
    let body = serde_json::to_vec(&record).context("serialize critic verdict")?;
    let blob_ref = BlobStore::open(resolved, &o.session)?.put(&body)?;
    let verdict_word = if !o.status.verified() {
        "unverified"
    } else if o.status.passed() {
        "passed"
    } else {
        "failed"
    };
    let artifact = Artifact {
        kind: "dispatch.critic_verdict".into(),
        summary: format!("critic {} p={:.2} → {verdict_word}", c.critic, c.p),
        content_type: "application/json".into(),
        // Numbers and names only (p, pass/fail, score, counts) — poolable.
        class: ArtifactClass::Signal,
        blob_ref,
        bytes: body.len() as u64,
        worker_id: o.session.clone(),
    };
    SessionLog::open(resolved, &o.session)?.append(&[Event::session(
        &o.session,
        Payload::Artifact(artifact),
    )
    .with_actor(Actor::service("dispatch"))])?;
    Ok(())
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
#[cfg(test)]
fn run_dispatch(
    driver: &dyn WorkerDriver,
    k: u32,
    prompt: &str,
    retries: u32,
    reward: &Grader,
    segments: Option<&[ResolvedSegment]>,
) -> DispatchVerdict {
    run_dispatch_with(
        driver,
        k,
        prompt,
        retries,
        reward,
        segments,
        None,
        CriticPolicy::Record,
    )
}

/// [`run_dispatch`] with an optional critic. Without one (`None`) this is the
/// pre-critic loop, byte-for-byte. With one, `policy` decides whether the
/// probabilities only ride along (`record`) or reorder / prune the verifier
/// runs (`order` / `select`, fork-`k` only).
#[allow(clippy::too_many_arguments)]
fn run_dispatch_with(
    driver: &dyn WorkerDriver,
    k: u32,
    prompt: &str,
    retries: u32,
    reward: &Grader,
    segments: Option<&[ResolvedSegment]>,
    critic: Option<&dyn Critic>,
    policy: CriticPolicy,
) -> DispatchVerdict {
    let ctx = critic.map(|c| CriticCtx { critic: c, prompt });
    // Fork all k up front, THEN drive each. Forking first overlaps the k VM
    // BOOTS (each `--detach` worker boots in the background). Server-mode worker
    // turns are driven SERIALLY below; one-shot CLI workers receive their only
    // prompt at launch and are merely awaited/graded below. True turn-level
    // concurrency would need driving workers on separate threads; the subprocess
    // `WorkerDriver` calls are independent, so that's a safe future change — not
    // done here. A fork that fails becomes an `Errored` worker rather than aborting
    // the batch — the successes are still driven, and no forked worker is left
    // unrecorded (the orphan a `collect::<Result>()?` would leak).
    //
    // Each worker runs EITHER the segment chain (`--segments`, one session) OR the
    // single-prompt + retry loop (fork-`k`). With both `-k>1` and `--segments`, the
    // k workers each run the full chain → best-of-k OVER segmented chains.
    let fork_first_turn = if segments.is_some() { "" } else { prompt };
    let forked: Vec<(usize, Result<String>)> = (0..k)
        .map(|i| {
            let i = i as usize;
            (i, driver.fork(i, fork_first_turn))
        })
        .collect();
    let workers: Vec<WorkerOutcome> = match (ctx.as_ref(), policy, segments) {
        // The critic reorders/prunes grading: every worker must be idle BEFORE the
        // first grade, so the drive is two-phase (settle all → grade in p order).
        (Some(cx), CriticPolicy::Order | CriticPolicy::Select, None) => {
            drive_ranked(driver, forked, prompt, retries, reward, cx, policy)
        }
        _ => forked
            .into_iter()
            .map(|(i, forked)| match forked {
                Ok(id) => match segments {
                    Some(segs) => drive_segments_with(
                        driver,
                        i,
                        id,
                        prompt,
                        segs,
                        reward,
                        retries,
                        ctx.as_ref(),
                    ),
                    None => drive_one_with(driver, i, id, prompt, retries, reward, ctx.as_ref()),
                },
                Err(e) => {
                    eprintln!("pillbox: worker fork failed: {e:#}");
                    fork_failed()
                }
            })
            .collect(),
    };

    let critic_run = ctx.as_ref().map(|cx| CriticRun {
        kind: cx.critic.name().to_string(),
        policy,
        model: cx.critic.model().to_string(),
        verifier_runs: workers.iter().map(WorkerOutcome::verifier_runs).sum(),
        verifier_runs_saved: workers
            .iter()
            .filter(|w| w.status == WorkerStatus::Unverified)
            .count() as u32,
    });

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
        critic: critic_run,
    }
}

/// The critic-ranked drive (`--critic-policy order|select`, fork-`k` only).
/// Phase 1: settle every fork to idle after its first turn — a critic must see
/// finished edits. Phase 2: score each with the critic. Phase 3: grade in
/// descending `p` — `order` stops at the first passer, `select` grades only the
/// first — and everything left ungraded becomes `Unverified`, its `p` retained
/// as the only evidence about it. A worker that fails its first grade still gets
/// its `--retries` before the loop moves on, so per-worker semantics match the
/// plain drive.
fn drive_ranked(
    driver: &dyn WorkerDriver,
    forked: Vec<(usize, Result<String>)>,
    prompt: &str,
    retries: u32,
    reward: &Grader,
    cx: &CriticCtx,
    policy: CriticPolicy,
) -> Vec<WorkerOutcome> {
    let n = forked.len();
    let mut outcomes: Vec<Option<WorkerOutcome>> = (0..n).map(|_| None).collect();
    let mut settled: Vec<(usize, usize, String, Option<CriticVerdict>)> = Vec::new();
    for (slot, (i, forked)) in forked.into_iter().enumerate() {
        match forked {
            Err(e) => {
                eprintln!("pillbox: worker fork failed: {e:#}");
                outcomes[slot] = Some(fork_failed());
            }
            Ok(id) => match settle(driver, i, &id, prompt) {
                Ok(()) => {
                    let verdict = cx.score(driver, &id);
                    settled.push((slot, i, id, verdict));
                }
                Err(e) => outcomes[slot] = Some(errored(id, e)),
            },
        }
    }
    // Descending p; an unscored worker (critic error) ranks last; ties keep fork
    // order (stable sort).
    settled.sort_by(|a, b| {
        let pa = a.3.as_ref().map(|v| v.p).unwrap_or(-1.0);
        let pb = b.3.as_ref().map(|v| v.p).unwrap_or(-1.0);
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut found_pass = false;
    let mut graded_any = false;
    for (slot, i, id, verdict) in settled {
        let grade_it = match policy {
            CriticPolicy::Order => !found_pass,
            CriticPolicy::Select => !graded_any,
            CriticPolicy::Record => true,
        };
        if !grade_it {
            outcomes[slot] = Some(unverified(id, verdict));
            continue;
        }
        graded_any = true;
        let outcome = match grade_from_idle(driver, i, &id, reward, retries) {
            Ok((grade, used)) => outcome_from_grade(&id, grade, used, verdict),
            Err(e) => errored(id, e),
        };
        found_pass |= outcome.status.passed();
        outcomes[slot] = Some(outcome);
    }
    outcomes
        .into_iter()
        .map(|o| o.expect("every fork slot is filled by one of the phases"))
        .collect()
}

/// Bring a freshly forked worker to idle after its first turn. One-shot CLI
/// agents consumed the prompt at fork (just wait); server agents are driven.
fn settle(driver: &dyn WorkerDriver, i: usize, id: &str, first_turn: &str) -> Result<()> {
    if !driver.first_turn_driven_on_fork(i) {
        driver.send(id, first_turn)?;
    }
    driver.wait_idle(id)
}

/// Grade an already-idle worker, with the same retry loop as [`drive_to_grade`]
/// (re-drive with the distilled failure summary up to `retries` times). One-shot
/// agents can't take a second turn, so they get exactly one grade.
fn grade_from_idle(
    driver: &dyn WorkerDriver,
    i: usize,
    id: &str,
    grader: &Grader,
    retries: u32,
) -> Result<(Scored, u32)> {
    let mut used = 0u32;
    loop {
        let grade = driver.grade(id, grader)?;
        if grade.passed || used >= retries || driver.first_turn_driven_on_fork(i) {
            return Ok((grade, used));
        }
        driver.send(id, &distill_feedback(&grade))?;
        driver.wait_idle(id)?;
        used += 1;
    }
}

/// A graded worker's outcome, with whatever the critic said about it.
fn outcome_from_grade(
    id: &str,
    grade: Scored,
    used: u32,
    critic: Option<CriticVerdict>,
) -> WorkerOutcome {
    WorkerOutcome {
        session: id.to_string(),
        score: Some(grade.score),
        retries_used: used,
        status: status_of(&grade),
        grade: Some(grade),
        segments: vec![],
        critic,
    }
}

/// A worker the critic policy left ungraded — no score, no grade, just `p`.
fn unverified(id: String, critic: Option<CriticVerdict>) -> WorkerOutcome {
    WorkerOutcome {
        session: id,
        score: None,
        retries_used: 0,
        status: WorkerStatus::Unverified,
        grade: None,
        segments: vec![],
        critic,
    }
}

/// The outcome for a fork that never produced a session.
fn fork_failed() -> WorkerOutcome {
    WorkerOutcome {
        session: String::new(),
        score: None,
        retries_used: 0,
        status: WorkerStatus::Errored,
        grade: None,
        segments: vec![],
        critic: None,
    }
}

/// Drive one worker to a terminal outcome. Errors are caught into an `Errored`
/// outcome (not propagated) so one worker's failure doesn't sink the others.
#[cfg(test)]
fn drive_one(
    driver: &dyn WorkerDriver,
    i: usize,
    id: String,
    prompt: &str,
    retries: u32,
    reward: &Grader,
) -> WorkerOutcome {
    drive_one_with(driver, i, id, prompt, retries, reward, None)
}

fn drive_one_with(
    driver: &dyn WorkerDriver,
    i: usize,
    id: String,
    prompt: &str,
    retries: u32,
    reward: &Grader,
    critic: Option<&CriticCtx>,
) -> WorkerOutcome {
    drive_one_inner(driver, i, &id, prompt, retries, reward, critic)
        .unwrap_or_else(|e| errored(id, e))
}

/// The shared `Errored` outcome for a worker whose drive raised (boot/drive/grade
/// error) — one worker's failure never sinks the batch.
fn errored(id: String, e: anyhow::Error) -> WorkerOutcome {
    eprintln!("pillbox: worker `{id}` errored: {e:#}");
    WorkerOutcome {
        session: id,
        score: None,
        retries_used: 0,
        status: WorkerStatus::Errored,
        grade: None,
        segments: vec![],
        critic: None,
    }
}

/// Drive a worker to a grade. One-shot CLI agents already consumed turn 1 at
/// fork, so they only wait + grade. Server-mode agents use the send → wait-idle
/// → grade-against-`grader` → retry loop: re-drive with the distilled failure
/// summary until the grade passes or the `retries` budget is spent → `(final
/// grade, retries used)`. A server agent treats a fork-baked positional as a
/// pre-fill hint, not an executed turn.
fn drive_to_grade(
    driver: &dyn WorkerDriver,
    i: usize,
    id: &str,
    first_turn: &str,
    grader: &Grader,
    retries: u32,
) -> Result<(Scored, u32)> {
    let (grade, used, _critic) =
        drive_to_grade_with(driver, i, id, first_turn, grader, retries, None)?;
    Ok((grade, used))
}

/// [`drive_to_grade`] with an optional critic scored right before EACH grade —
/// the critic predicts *this* verifier call, so every attempt yields a labeled
/// pair. The returned verdict is the one aligned with the returned (final) grade.
fn drive_to_grade_with(
    driver: &dyn WorkerDriver,
    i: usize,
    id: &str,
    first_turn: &str,
    grader: &Grader,
    retries: u32,
    critic: Option<&CriticCtx>,
) -> Result<(Scored, u32, Option<CriticVerdict>)> {
    if driver.first_turn_driven_on_fork(i) {
        driver.wait_idle(id)?;
        let verdict = critic.and_then(|c| c.score(driver, id));
        let grade = driver.grade(id, grader)?;
        return Ok((grade, 0, verdict));
    }

    let mut turn = first_turn.to_string();
    let mut used = 0u32;
    loop {
        driver.send(id, &turn)?;
        driver.wait_idle(id)?;
        let verdict = critic.and_then(|c| c.score(driver, id));
        let grade = driver.grade(id, grader)?;
        if grade.passed || used >= retries {
            return Ok((grade, used, verdict));
        }
        turn = distill_feedback(&grade);
        used += 1;
    }
}

/// `Scored` → terminal worker status — the single mapping, so the two drive paths
/// can't disagree on what a pass/fail looks like.
fn status_of(grade: &Scored) -> WorkerStatus {
    if grade.passed {
        WorkerStatus::Scored
    } else {
        WorkerStatus::Failed
    }
}

/// Drive one fork-`k` worker (single prompt, graded by the reward) to its terminal
/// outcome.
fn drive_one_inner(
    driver: &dyn WorkerDriver,
    i: usize,
    id: &str,
    prompt: &str,
    retries: u32,
    reward: &Grader,
    critic: Option<&CriticCtx>,
) -> Result<WorkerOutcome> {
    let (grade, used, verdict) =
        drive_to_grade_with(driver, i, id, prompt, reward, retries, critic)?;
    Ok(outcome_from_grade(id, grade, used, verdict))
}

/// Drive one worker through a SEGMENT CHAIN to a terminal outcome (errors → an
/// `Errored` outcome, like [`drive_one`]).
#[cfg(test)]
fn drive_segments(
    driver: &dyn WorkerDriver,
    i: usize,
    id: String,
    context: &str,
    segments: &[ResolvedSegment],
    reward: &Grader,
    retries: u32,
) -> WorkerOutcome {
    drive_segments_with(driver, i, id, context, segments, reward, retries, None)
}

#[allow(clippy::too_many_arguments)]
fn drive_segments_with(
    driver: &dyn WorkerDriver,
    i: usize,
    id: String,
    context: &str,
    segments: &[ResolvedSegment],
    reward: &Grader,
    retries: u32,
    critic: Option<&CriticCtx>,
) -> WorkerOutcome {
    drive_segments_inner(driver, i, &id, context, segments, reward, retries, critic)
        .unwrap_or_else(|e| errored(id, e))
}

/// Drive the focused per-segment sub-prompts SEQUENTIALLY in ONE session (the
/// proven `chained` lever, docs/optimization-gate.md §2026-06-19) — context
/// accumulates, the horizon never resets. Each segment: send its prompt →
/// wait-idle → grade against its **gate** → on a failed gate with budget left,
/// re-drive with the distilled summary (per-segment `retries`). A failed gate
/// does NOT abort the chain — it advances and lets the run-level **reward** be the
/// authoritative final grade (matches the harness's best-effort progression). The
/// worker's `retries_used` is the sum across segments.
#[allow(clippy::too_many_arguments)]
fn drive_segments_inner(
    driver: &dyn WorkerDriver,
    worker_i: usize,
    id: &str,
    context: &str,
    segments: &[ResolvedSegment],
    reward: &Grader,
    retries: u32,
    critic: Option<&CriticCtx>,
) -> Result<WorkerOutcome> {
    let mut seg_outcomes = Vec::with_capacity(segments.len());
    for (i, seg) in segments.iter().enumerate() {
        // Optional positional context rides the FIRST segment's prompt only.
        let turn = if i == 0 && !context.is_empty() {
            format!("{context}\n\n{}", seg.prompt)
        } else {
            seg.prompt.clone()
        };
        let (grade, used) = drive_to_grade(driver, worker_i, id, &turn, &seg.gate, retries)?;
        seg_outcomes.push(SegmentOutcome {
            name: seg.name.clone(),
            passed: grade.passed,
            score: grade.score,
            retries_used: used,
        });
    }
    // The critic judges the finished chain, right before the reward — the gates
    // are the chain's own business.
    let verdict = critic.and_then(|c| c.score(driver, id));
    // Authoritative final grade = the run-level reward (distinct from the gates),
    // graded once after the chain — no retry.
    let final_grade = driver.grade(id, reward)?;
    let retries_used = seg_outcomes.iter().map(|s| s.retries_used).sum();
    Ok(WorkerOutcome {
        session: id.to_string(),
        score: Some(final_grade.score),
        retries_used,
        status: status_of(&final_grade),
        grade: Some(final_grade),
        segments: seg_outcomes,
        critic: verdict,
    })
}

// ── the live CLI driver (subprocess self-exec) ──────────────────────────────

/// Drives workers by self-exec'ing the pillbox binary and parsing the documented
/// `--json` contracts. All subprocesses inherit cwd, so they resolve the same
/// pillbox as the parent (the session a `fork` creates is found by a later
/// `grade`/`pull`).
struct CliDriver<'a> {
    exe: PathBuf,
    opts: &'a DispatchOpts,
    default_agent: String,
    /// Durable dir the winner is pulled into (a TempDir would drop it).
    rundir: PathBuf,
}

impl<'a> CliDriver<'a> {
    fn new(opts: &'a DispatchOpts, default_agent: String) -> Result<Self> {
        let exe = std::env::current_exe().context("locate the pillbox binary")?;
        let rundir = std::env::temp_dir().join(format!(
            "pillbox-dispatch-{}",
            uuid::Uuid::now_v7().simple()
        ));
        Ok(Self {
            exe,
            opts,
            default_agent,
            rundir,
        })
    }

    fn effective_agent(&self, i: usize) -> String {
        effective_worker_agent(self.opts, i, &self.default_agent)
    }
}

impl WorkerDriver for CliDriver<'_> {
    fn fork(&self, i: usize, first_turn: &str) -> Result<String> {
        // Per-worker agent/model/temperature from the roster (`--workers-spec`)
        // falling back to the run-level scalars; without a roster this is the
        // scalar opts for every worker. The argv assembly below is otherwise
        // unchanged — only these values become per-worker.
        let (agent, model, temperature) = resolve_worker(self.opts, i);
        let mut args = vec![
            "run".into(),
            "--from-bookmark".into(),
            self.opts.from_bookmark.clone(),
            "--detach".into(),
            "--json".into(),
        ];
        if let Some(a) = &agent {
            args.extend(["--agent".into(), a.clone()]);
        }
        if let Some(m) = &model {
            args.extend(["--model".into(), m.clone()]);
        }
        if let Some(t) = &temperature {
            args.extend(["--temperature".into(), t.to_string()]);
        }
        if self.opts.memory {
            args.push("--memory".into());
        }
        if let Some(t) = &self.opts.ttl {
            // `run --ttl` requires `--detach` (already passed above) and writes
            // `expires_at` so `session prune` can later reap this worker.
            args.extend(["--ttl".into(), t.clone()]);
        }
        if self.first_turn_driven_on_fork(i) {
            args.extend(["--".into(), first_turn.into()]);
        }
        // Server-mode workers get no positional prompt: a `--detach` server comes
        // up idle; the segment prompt is driven as turn 1 via `session send` (see
        // `drive_one_inner`). A server treats a baked positional as a pre-fill
        // hint, not a turn.
        let out = self.capture(&args)?;
        let v: serde_json::Value = serde_json::from_str(&out)
            .with_context(|| format!("parse `run --json` output: {out:?}"))?;
        v["session"]["id"]
            .as_str()
            .map(str::to_string)
            .context("`run --json` had no session.id")
    }

    fn first_turn_driven_on_fork(&self, i: usize) -> bool {
        agent_first_turn_driven_on_fork(&self.effective_agent(i))
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

    fn grade(&self, id: &str, grader: &Grader) -> Result<Scored> {
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
        args.extend(grader.flags());
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

    fn edits(&self, id: &str) -> Result<Vec<EditRecord>> {
        // The worker's own §0 log is the source — the same events the offline
        // harness extracts, so the live critic sees exactly what was measured.
        let out = self.capture(&[
            "session".into(),
            "log".into(),
            id.into(),
            "--type".into(),
            "tool_call".into(),
        ])?;
        Ok(edits_from_log(&out))
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

// ── segment spec (`--segments`, TOML) ───────────────────────────────────────

/// One `[[segment]]` in the `--segments` TOML: a focused sub-prompt (inline
/// `prompt` xor `prompt_file`) and a gate (`gate_cmd` xor `gate_rubric`). The
/// gate steers progression; the run-level `--rubric`/`--cmd` reward is the
/// authoritative final grade. `deny_unknown_fields` so a typo'd key is a loud
/// parse error, not silently ignored.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentSpec {
    name: String,
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
    gate_rubric: Option<PathBuf>,
    gate_cmd: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentsFile {
    #[serde(default)]
    segment: Vec<SegmentSpec>,
}

/// A segment with its prompt read and gate resolved — ready to drive.
struct ResolvedSegment {
    name: String,
    prompt: String,
    gate: Grader,
}

/// Load + validate the `--segments` TOML spec. Relative `prompt_file`/`gate_rubric`
/// paths resolve against the spec file's directory. Every failure is a
/// [`PillboxError::usage`] (exit 2) so a bad spec fails fast, before any worker is
/// forked — not mid-dispatch.
fn load_segments(spec_path: &std::path::Path) -> Result<Vec<ResolvedSegment>> {
    let usage = |msg: String| -> anyhow::Error { PillboxError::usage("dispatch", msg).into() };
    let raw = std::fs::read_to_string(spec_path)
        .map_err(|e| usage(format!("read --segments {}: {e}", spec_path.display())))?;
    let file: SegmentsFile = toml::from_str(&raw)
        .map_err(|e| usage(format!("parse --segments {}: {e}", spec_path.display())))?;
    if file.segment.is_empty() {
        return Err(PillboxError::usage(
            "dispatch",
            format!(
                "--segments {} has no [[segment]] entries",
                spec_path.display()
            ),
        )
        .with_next("each [[segment]] needs name + (prompt|prompt_file) + (gate_rubric|gate_cmd)")
        .into());
    }
    let dir = spec_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let resolve = |p: &PathBuf| -> PathBuf {
        if p.is_absolute() {
            p.clone()
        } else {
            dir.join(p)
        }
    };
    let mut out = Vec::with_capacity(file.segment.len());
    for s in &file.segment {
        let prompt = match (&s.prompt, &s.prompt_file) {
            (Some(p), None) => p.clone(),
            (None, Some(f)) => {
                let path = resolve(f);
                std::fs::read_to_string(&path).map_err(|e| {
                    usage(format!(
                        "segment `{}`: read prompt_file {}: {e}",
                        s.name,
                        path.display()
                    ))
                })?
            }
            _ => {
                return Err(usage(format!(
                    "segment `{}` needs exactly one of `prompt` or `prompt_file`",
                    s.name
                )))
            }
        };
        let gate = match (&s.gate_cmd, &s.gate_rubric) {
            (Some(c), None) => Grader::Cmd(c.clone()),
            (None, Some(r)) => Grader::Rubric(resolve(r)),
            _ => {
                return Err(usage(format!(
                    "segment `{}` needs exactly one of `gate_cmd` or `gate_rubric`",
                    s.name
                )))
            }
        };
        out.push(ResolvedSegment {
            name: s.name.clone(),
            prompt,
            gate,
        });
    }
    Ok(out)
}

// ── worker roster (`--workers-spec`, TOML) ──────────────────────────────────

/// One `[[worker]]` in the `--workers-spec` TOML: a per-worker agent/model/
/// temperature binding for the heterogeneous-roster fork-`k`. Every field is
/// optional — an omitted field falls back to the run-level `--agent`/`--model`/
/// `--temperature` (themselves falling back to `pillbox.toml`). `deny_unknown_fields`
/// so a typo'd key is a loud parse error, not silently ignored.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerSpec {
    agent: Option<String>,
    model: Option<String>,
    temperature: Option<f64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkersFile {
    #[serde(default)]
    worker: Vec<WorkerSpec>,
}

/// Load + validate the `--workers-spec` TOML roster. An empty roster is a
/// [`PillboxError::usage`] (exit 2) so a bad spec fails fast, before any worker
/// is forked — mirroring [`load_segments`]' empty-spec rejection. Called from
/// `main.rs` to materialize [`DispatchOpts::workers_spec`] (the parsed roster).
pub(crate) fn load_workers_spec(path: &std::path::Path) -> Result<Vec<WorkerSpec>> {
    let usage = |msg: String| -> anyhow::Error { PillboxError::usage("dispatch", msg).into() };
    let raw = std::fs::read_to_string(path)
        .map_err(|e| usage(format!("read --workers-spec {}: {e}", path.display())))?;
    let file: WorkersFile = toml::from_str(&raw)
        .map_err(|e| usage(format!("parse --workers-spec {}: {e}", path.display())))?;
    if file.worker.is_empty() {
        return Err(PillboxError::usage(
            "dispatch",
            format!(
                "--workers-spec {} has no [[worker]] entries",
                path.display()
            ),
        )
        .with_next("each [[worker]] may set agent / model / temperature (all optional)")
        .into());
    }
    Ok(file.worker)
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
    let prompt = opts.prompt.join(" ");
    // fork-`k` needs a positional prompt (the work). `--segments` carries the work
    // in the spec, so there the positional is optional context.
    if opts.segments.is_none() && prompt.trim().is_empty() {
        return Err(PillboxError::usage(
            "dispatch",
            "no segment prompt — workers have nothing to do",
        )
        .with_next("pillbox dispatch … -- \"<the segment prompt>\"")
        .into());
    }
    // Validate `--ttl` at the boundary so a bad duration fails fast (exit 2)
    // instead of after forking k workers (each `run --ttl` would reject it).
    if let Some(t) = &opts.ttl {
        crate::session::parse_ttl_seconds(t).map_err(|e| {
            PillboxError::usage("dispatch", format!("invalid --ttl `{t}`: {e}"))
                .with_next("use a duration like 30m / 24h / 7d")
        })?;
    }

    // With a `--workers-spec` roster, its length is the authoritative `k`. An
    // explicit `-k N` that disagrees with the roster is a usage error (exit 2)
    // before any fork — but `-k` left at its default just derives `k` from the
    // roster. Without a roster, `-k` is used as-is (today's homogeneous path).
    let k = match &opts.workers_spec {
        Some(roster) => {
            if opts.workers != DEFAULT_WORKERS && opts.workers as usize != roster.len() {
                return Err(PillboxError::usage(
                    "dispatch",
                    format!(
                        "-k {} disagrees with --workers-spec ({} [[worker]] entries)",
                        opts.workers,
                        roster.len()
                    ),
                )
                .with_next("drop -k (the roster length is authoritative) or match it to the roster")
                .into());
            }
            roster.len() as u32
        }
        None => opts.workers,
    };

    let default_agent_id = default_agent(resolved);
    if opts.segments.is_some()
        && (0..k).any(|i| worker_first_turn_driven_on_fork(&opts, i as usize, &default_agent_id))
    {
        return Err(PillboxError::usage(
            "dispatch",
            "`--segments` requires a server-mode agent (opencode); claude/codex are one-shot — use best-of-k (`-k`) instead.",
        )
        .into());
    }

    // The run-level reward — the authoritative final grade, distinct from any
    // per-segment gate.
    let reward = Grader::from_opts(&opts)?;
    // Parse + validate the segment spec up front (exit 2 on a bad spec) — before
    // forking any worker.
    let segments = match &opts.segments {
        Some(p) => Some(load_segments(p)?),
        None => None,
    };

    // The critic (`--critic`): resolved and credentialed up front, so a missing
    // key or a bad policy is a usage error before any VM boots. `order`/`select`
    // reorder the grading of independent forks; a segment chain grades once at
    // the end, so there they'd be meaningless — `record` is the only policy that
    // composes with `--segments`.
    let critic_policy = CriticPolicy::parse(&opts.critic_policy)?;
    let critic: Option<TypesafeCritic> = match opts.critic.as_deref() {
        None => {
            if critic_policy != CriticPolicy::Record {
                return Err(PillboxError::usage(
                    "dispatch",
                    format!("--critic-policy {} needs --critic", critic_policy.as_str()),
                )
                .with_next("add --critic typesafe")
                .into());
            }
            None
        }
        Some("typesafe") => Some(TypesafeCritic::from_env(&opts.critic_model)?),
        Some(other) => {
            return Err(
                PillboxError::usage("dispatch", format!("unknown --critic `{other}`"))
                    .with_next("use --critic typesafe")
                    .into(),
            )
        }
    };
    if critic.is_some() && segments.is_some() && critic_policy != CriticPolicy::Record {
        return Err(PillboxError::usage(
            "dispatch",
            "`--critic-policy order|select` reorders fork-k grading; with --segments use --critic-policy record",
        )
        .into());
    }

    let driver = CliDriver::new(&opts, default_agent_id)?;
    let verdict = run_dispatch_with(
        &driver,
        k,
        &prompt,
        opts.retries,
        &reward,
        segments.as_deref(),
        critic.as_ref().map(|c| c as &dyn Critic),
        critic_policy,
    );

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
            segments: vec![],
            critic: None,
        }
    }

    /// A throwaway reward grader for the loop tests (the mock ignores the grader
    /// and replays its scripted grade queue).
    fn reward() -> Grader {
        Grader::Cmd("reward".into())
    }

    /// A scripted critic: `p` per worker id. It finds the id in the rendered
    /// state (the mock driver's single edit names the worker), so the policy
    /// under test sees the real render → score path.
    struct MockCritic {
        ps: std::collections::HashMap<String, f64>,
        calls: RefCell<Vec<String>>,
    }

    impl MockCritic {
        fn new(ps: &[(&str, f64)]) -> Self {
            Self {
                ps: ps.iter().map(|(id, p)| (id.to_string(), *p)).collect(),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Critic for MockCritic {
        fn name(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock-1"
        }
        fn score(&self, state: &str) -> Result<CriticVerdict> {
            let id = state
                .split("## edit 1: edit ")
                .nth(1)
                .and_then(|s| s.lines().next())
                .expect("mock state names the worker");
            self.calls.borrow_mut().push(id.to_string());
            let p = *self.ps.get(id).expect("scripted p for worker");
            Ok(CriticVerdict {
                critic: "mock".into(),
                model: "mock-1".into(),
                p,
                latency_ms: 1,
                cost_usd: Some(0.0),
                input_tokens: 1,
                truncated: false,
                n_edits: 1,
            })
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
        /// The fork-index passed to each `fork`, in call order — asserts the loop
        /// hands each worker its own 0-based roster index.
        fork_indices: RefCell<Vec<usize>>,
        first_turn_on_fork: bool,
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
                fork_indices: RefCell::new(Vec::new()),
                first_turn_on_fork: false,
            }
        }
        fn failing_grade(mut self, id: &str) -> Self {
            self.grade_errs.insert(id.to_string());
            self
        }
        fn first_turn_on_fork(mut self) -> Self {
            self.first_turn_on_fork = true;
            self
        }
    }

    impl WorkerDriver for MockDriver {
        fn fork(&self, i: usize, _first_turn: &str) -> Result<String> {
            self.fork_indices.borrow_mut().push(i);
            Ok(self.ids.borrow_mut().pop_front().expect("fork over budget"))
        }
        fn first_turn_driven_on_fork(&self, _i: usize) -> bool {
            self.first_turn_on_fork
        }
        fn wait_idle(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        fn grade(&self, id: &str, _grader: &Grader) -> Result<Scored> {
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
        fn edits(&self, id: &str) -> Result<Vec<EditRecord>> {
            // One edit that names the worker, so a MockCritic can find it in the
            // rendered state.
            Ok(vec![EditRecord {
                tool: "edit".into(),
                file: id.into(),
                old: String::new(),
                new: format!("// {id}"),
            }])
        }
    }

    // ── the critic policies (`--critic`) ──

    #[test]
    fn record_policy_attaches_p_and_leaves_grading_unchanged() {
        // Same script as `loop_selects_winner_and_pulls_it`: the critic must not
        // change a single send, grade, or the winner — it only rides along.
        let d = MockDriver::new(vec![
            ("w0", vec![(false, 0.5), (true, 1.0)]),
            ("w1", vec![(true, 1.0)]),
        ]);
        let c = MockCritic::new(&[("w0", 0.2), ("w1", 0.9)]);
        let v = run_dispatch_with(
            &d,
            2,
            "task",
            2,
            &reward(),
            None,
            Some(&c),
            CriticPolicy::Record,
        );
        assert_eq!(v.winner.as_deref(), Some("w1"));
        assert_eq!(*d.sends.borrow(), 3);
        assert_eq!(v.workers[0].critic.as_ref().map(|c| c.p), Some(0.2));
        assert_eq!(v.workers[1].critic.as_ref().map(|c| c.p), Some(0.9));
        // Scored before EACH grade: w0 twice (fail, then pass), w1 once.
        assert_eq!(*c.calls.borrow(), vec!["w0", "w0", "w1"]);
        let run = v.critic.as_ref().expect("critic block");
        assert_eq!(run.policy, CriticPolicy::Record);
        assert_eq!(run.verifier_runs, 3);
        assert_eq!(run.verifier_runs_saved, 0);
        assert!(v.workers.iter().all(|w| w.status.verified()));
    }

    #[test]
    fn order_policy_grades_by_descending_p_and_stops_at_the_first_passer() {
        // All three would pass; the critic ranks w1 first, so only w1 is graded.
        let d = MockDriver::new(vec![
            ("w0", vec![(true, 1.0)]),
            ("w1", vec![(true, 1.0)]),
            ("w2", vec![(true, 1.0)]),
        ]);
        let c = MockCritic::new(&[("w0", 0.1), ("w1", 0.9), ("w2", 0.5)]);
        let v = run_dispatch_with(
            &d,
            3,
            "task",
            1,
            &reward(),
            None,
            Some(&c),
            CriticPolicy::Order,
        );
        assert_eq!(v.winner.as_deref(), Some("w1"));
        assert_eq!(v.workers[1].status, WorkerStatus::Scored);
        assert_eq!(v.workers[0].status, WorkerStatus::Unverified);
        assert_eq!(v.workers[2].status, WorkerStatus::Unverified);
        // Unverified workers keep their p and cost no verifier run.
        assert_eq!(v.workers[0].critic.as_ref().map(|c| c.p), Some(0.1));
        assert_eq!(v.workers[0].score, None);
        assert_eq!(
            d.grades.borrow()["w0"].len(),
            1,
            "w0's grade was never consumed"
        );
        assert_eq!(
            d.grades.borrow()["w2"].len(),
            1,
            "w2's grade was never consumed"
        );
        let run = v.critic.as_ref().unwrap();
        assert_eq!(run.verifier_runs, 1);
        assert_eq!(run.verifier_runs_saved, 2);
        // Every fork was settled (one send each) before any grade.
        assert_eq!(*d.sends.borrow(), 3);
        // Outcomes stay in fork order regardless of grading order.
        assert_eq!(
            v.workers
                .iter()
                .map(|w| w.session.as_str())
                .collect::<Vec<_>>(),
            ["w0", "w1", "w2"]
        );
    }

    #[test]
    fn order_policy_falls_through_when_the_top_ranked_worker_fails() {
        // The critic is wrong about w0 (0.9 but fails, no retries); w1 passes.
        let d = MockDriver::new(vec![("w0", vec![(false, 0.0)]), ("w1", vec![(true, 1.0)])]);
        let c = MockCritic::new(&[("w0", 0.9), ("w1", 0.4)]);
        let v = run_dispatch_with(
            &d,
            2,
            "task",
            0,
            &reward(),
            None,
            Some(&c),
            CriticPolicy::Order,
        );
        assert_eq!(v.winner.as_deref(), Some("w1"));
        assert_eq!(v.workers[0].status, WorkerStatus::Failed);
        assert_eq!(v.workers[1].status, WorkerStatus::Scored);
        let run = v.critic.as_ref().unwrap();
        assert_eq!(run.verifier_runs, 2);
        assert_eq!(run.verifier_runs_saved, 0);
        // The rationale shows the critic's p next to the verifier's decision.
        assert!(v
            .selection_rationale
            .as_deref()
            .unwrap()
            .contains("critic p=0.40"));
    }

    #[test]
    fn order_policy_retries_a_failed_worker_before_moving_on() {
        // w0 ranked first, fails once then passes within its retry budget → it
        // wins and w1 is never graded.
        let d = MockDriver::new(vec![
            ("w0", vec![(false, 0.5), (true, 1.0)]),
            ("w1", vec![(true, 1.0)]),
        ]);
        let c = MockCritic::new(&[("w0", 0.8), ("w1", 0.7)]);
        let v = run_dispatch_with(
            &d,
            2,
            "task",
            1,
            &reward(),
            None,
            Some(&c),
            CriticPolicy::Order,
        );
        assert_eq!(v.winner.as_deref(), Some("w0"));
        assert_eq!(v.workers[0].retries_used, 1);
        assert_eq!(v.workers[1].status, WorkerStatus::Unverified);
        // sends: 2 settles + 1 retry re-drive.
        assert_eq!(*d.sends.borrow(), 3);
        assert_eq!(v.critic.as_ref().unwrap().verifier_runs, 2);
    }

    #[test]
    fn select_policy_grades_only_the_argmax_and_can_end_with_no_winner() {
        // The critic picks w0 (0.9); it fails. w1 would have passed but is never
        // graded — the honest cost of trusting the critic.
        let d = MockDriver::new(vec![("w0", vec![(false, 0.0)]), ("w1", vec![(true, 1.0)])]);
        let c = MockCritic::new(&[("w0", 0.9), ("w1", 0.3)]);
        let v = run_dispatch_with(
            &d,
            2,
            "task",
            0,
            &reward(),
            None,
            Some(&c),
            CriticPolicy::Select,
        );
        assert_eq!(v.winner, None);
        assert_eq!(v.workers[0].status, WorkerStatus::Failed);
        assert_eq!(v.workers[1].status, WorkerStatus::Unverified);
        assert_eq!(d.grades.borrow()["w1"].len(), 1);
        let run = v.critic.as_ref().unwrap();
        assert_eq!(run.verifier_runs, 1);
        assert_eq!(run.verifier_runs_saved, 1);
        assert!(d.pulls.borrow().is_empty());
    }

    #[test]
    fn critic_failure_leaves_the_worker_unscored_but_graded() {
        // A critic that errors must never turn a gradeable worker into Errored.
        struct BrokenCritic;
        impl Critic for BrokenCritic {
            fn name(&self) -> &str {
                "broken"
            }
            fn model(&self) -> &str {
                "none"
            }
            fn score(&self, _state: &str) -> Result<CriticVerdict> {
                bail!("HTTP 529")
            }
        }
        let d = MockDriver::new(vec![("w0", vec![(true, 1.0)])]);
        let v = run_dispatch_with(
            &d,
            1,
            "task",
            0,
            &reward(),
            None,
            Some(&BrokenCritic),
            CriticPolicy::Order,
        );
        assert_eq!(v.winner.as_deref(), Some("w0"));
        assert_eq!(v.workers[0].status, WorkerStatus::Scored);
        assert!(v.workers[0].critic.is_none());
    }

    #[test]
    fn verdict_json_carries_critic_blocks_only_when_a_critic_ran() {
        let plain = run_dispatch(
            &MockDriver::new(vec![("w0", vec![(true, 1.0)])]),
            1,
            "task",
            0,
            &reward(),
            None,
        );
        let val = plain.to_json_value();
        assert!(val.get("critic").is_none());
        assert!(val["workers"][0].get("critic").is_none());

        let d = MockDriver::new(vec![("w0", vec![(true, 1.0)])]);
        let c = MockCritic::new(&[("w0", 0.77)]);
        let with = run_dispatch_with(
            &d,
            1,
            "task",
            0,
            &reward(),
            None,
            Some(&c),
            CriticPolicy::Record,
        );
        let val = with.to_json_value();
        assert_eq!(val["critic"]["kind"], "mock");
        assert_eq!(val["critic"]["policy"], "record");
        assert_eq!(val["critic"]["verifier_runs"], 1);
        assert_eq!(val["workers"][0]["critic"]["p"], 0.77);
        assert_eq!(val["workers"][0]["critic"]["n_edits"], 1);
    }

    #[test]
    fn unverified_never_wins_and_costs_no_verifier_run() {
        let ws = vec![
            outcome("a", None, 0, WorkerStatus::Unverified),
            outcome("b", Some(0.9), 0, WorkerStatus::Failed),
        ];
        assert_eq!(select_winner(&ws), None);
        assert_eq!(ws[0].verifier_runs(), 0);
        assert_eq!(ws[1].verifier_runs(), 1);
        assert_eq!(
            serde_json::to_value(WorkerStatus::Unverified).unwrap(),
            "unverified"
        );
    }

    #[test]
    fn loop_selects_winner_and_pulls_it() {
        // worker "w0" fails then passes (1 retry); "w1" passes first try.
        let d = MockDriver::new(vec![
            ("w0", vec![(false, 0.5), (true, 1.0)]),
            ("w1", vec![(true, 1.0)]),
        ]);
        let v = run_dispatch(&d, 2, "task", 2, &reward(), None);
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
        let v = run_dispatch(&d, 2, "task", 0, &reward(), None);
        assert_eq!(v.workers[0].status, WorkerStatus::Errored);
        assert_eq!(v.workers[0].score, None);
        assert_eq!(v.winner.as_deref(), Some("good"));
        assert_eq!(*d.pulls.borrow(), vec!["good".to_string()]);
    }

    #[test]
    fn loop_respects_retry_budget_then_fails() {
        // never passes; budget 1 → 1 retry then Failed, no winner, no pull.
        let d = MockDriver::new(vec![("w0", vec![(false, 0.3), (false, 0.4)])]);
        let v = run_dispatch(&d, 1, "task", 1, &reward(), None);
        assert_eq!(v.workers[0].status, WorkerStatus::Failed);
        assert_eq!(v.workers[0].retries_used, 1);
        assert_eq!(v.workers[0].score, Some(0.4));
        assert_eq!(v.winner, None);
        assert_eq!(v.pulled_to, None);
        // 2 sends: the turn-1 prompt + one retry.
        assert_eq!(*d.sends.borrow(), 2);
        assert!(d.pulls.borrow().is_empty());
    }

    #[test]
    fn one_shot_drive_one_grades_fork_result_without_send_or_retry() {
        let d = MockDriver::new(vec![("w0", vec![(false, 0.3)])]).first_turn_on_fork();
        let w = drive_one(&d, 0, "w0".into(), "task", 3, &reward());
        assert_eq!(w.session, "w0");
        assert_eq!(w.status, WorkerStatus::Failed);
        assert_eq!(w.retries_used, 0);
        assert_eq!(w.score, Some(0.3));
        assert_eq!(*d.sends.borrow(), 0);
        assert!(d.pulls.borrow().is_empty());
    }

    #[test]
    fn loop_forks_each_worker_with_its_index() {
        // The loop must hand each fork its own 0-based index (the roster row a
        // `--workers-spec` worker binds to), in order.
        let d = MockDriver::new(vec![
            ("w0", vec![(true, 1.0)]),
            ("w1", vec![(true, 1.0)]),
            ("w2", vec![(true, 1.0)]),
        ]);
        let _ = run_dispatch(&d, 3, "task", 0, &reward(), None);
        assert_eq!(*d.fork_indices.borrow(), vec![0, 1, 2]);
    }

    // ── segment chain (`--segments`) ──

    #[test]
    fn loop_drives_segment_chain_then_final_reward_in_one_session() {
        // ONE worker, 2 segments + the final reward, all in one session. seg "a"
        // passes first try; seg "b" fails its gate once then passes (1 retry); the
        // final reward passes. The mock's grade queue is consumed in order:
        // [a-gate, b-gate(fail), b-gate(pass), reward].
        let segs = vec![
            ResolvedSegment {
                name: "a".into(),
                prompt: "do a".into(),
                gate: Grader::Cmd("ga".into()),
            },
            ResolvedSegment {
                name: "b".into(),
                prompt: "do b".into(),
                gate: Grader::Cmd("gb".into()),
            },
        ];
        let d = MockDriver::new(vec![(
            "w0",
            vec![(true, 1.0), (false, 0.5), (true, 1.0), (true, 1.0)],
        )]);
        let v = run_dispatch(&d, 1, "", 1, &reward(), Some(&segs));
        let w = &v.workers[0];
        assert_eq!(w.status, WorkerStatus::Scored);
        assert_eq!(w.score, Some(1.0));
        // Per-segment trajectory captured.
        assert_eq!(w.segments.len(), 2);
        assert_eq!(w.segments[0].name, "a");
        assert_eq!(w.segments[0].retries_used, 0);
        assert!(w.segments[0].passed);
        assert_eq!(w.segments[1].name, "b");
        assert_eq!(w.segments[1].retries_used, 1);
        assert!(w.segments[1].passed);
        // Worker retries = SUM across segments.
        assert_eq!(w.retries_used, 1);
        // sends: a (1) + b (1 + 1 retry) = 3; the final reward grade does not send.
        assert_eq!(*d.sends.borrow(), 3);
        assert_eq!(v.winner.as_deref(), Some("w0"));
        assert_eq!(*d.pulls.borrow(), vec!["w0".to_string()]);
        // The verdict JSON carries the segments array for a segmented worker.
        let val = v.to_json_value();
        let seg = &val["workers"][0]["segments"];
        assert_eq!(seg.as_array().unwrap().len(), 2);
        assert_eq!(seg[1]["name"], "b");
        assert_eq!(seg[1]["retries_used"], 1);
        assert_eq!(seg[1]["passed"], true);
    }

    #[test]
    fn segment_chain_advances_past_a_failed_gate_and_reward_decides() {
        // seg "a" never passes its gate (budget 0 → 1 attempt, fails); the chain
        // STILL advances to seg "b" and the final reward decides the worker. Here
        // the reward fails → the worker is Failed, no winner.
        let segs = vec![
            ResolvedSegment {
                name: "a".into(),
                prompt: "do a".into(),
                gate: Grader::Cmd("ga".into()),
            },
            ResolvedSegment {
                name: "b".into(),
                prompt: "do b".into(),
                gate: Grader::Cmd("gb".into()),
            },
        ];
        let d = MockDriver::new(vec![(
            "w0",
            vec![(false, 0.0), (true, 1.0), (false, 0.4)], // a-gate fail, b-gate pass, reward fail
        )]);
        let v = run_dispatch(&d, 1, "", 0, &reward(), Some(&segs));
        let w = &v.workers[0];
        assert_eq!(w.segments.len(), 2, "advanced past the failed gate");
        assert!(!w.segments[0].passed);
        assert!(w.segments[1].passed);
        assert_eq!(w.status, WorkerStatus::Failed); // reward is authoritative
        assert_eq!(w.score, Some(0.4));
        assert_eq!(v.winner, None);
        // sends: a (1) + b (1) = 2 (no retries at budget 0).
        assert_eq!(*d.sends.borrow(), 2);
    }

    #[test]
    fn load_segments_parses_and_resolves() {
        let dir = std::env::temp_dir().join(format!("pb-seg-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("p1.txt"), "focused prompt 1").unwrap();
        std::fs::write(dir.join("r1.txt"), "c :: true").unwrap();
        let spec = dir.join("segs.toml");
        std::fs::write(
            &spec,
            "[[segment]]\nname = \"one\"\nprompt_file = \"p1.txt\"\ngate_rubric = \"r1.txt\"\n\n\
             [[segment]]\nname = \"two\"\nprompt = \"inline 2\"\ngate_cmd = \"pytest -k two\"\n",
        )
        .unwrap();
        let segs = load_segments(&spec).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].name, "one");
        assert_eq!(segs[0].prompt, "focused prompt 1"); // read from file, rel to spec dir
        assert!(matches!(&segs[0].gate, Grader::Rubric(p) if p.ends_with("r1.txt")));
        assert_eq!(segs[1].prompt, "inline 2");
        assert!(matches!(&segs[1].gate, Grader::Cmd(c) if c == "pytest -k two"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_segments_rejects_both_prompt_sources() {
        let dir = std::env::temp_dir().join(format!("pb-seg-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let spec = dir.join("bad.toml");
        std::fs::write(
            &spec,
            "[[segment]]\nname = \"x\"\nprompt = \"a\"\nprompt_file = \"b.txt\"\ngate_cmd = \"true\"\n",
        )
        .unwrap();
        assert!(load_segments(&spec).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_segments_rejects_empty_spec() {
        let dir = std::env::temp_dir().join(format!("pb-seg-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let spec = dir.join("empty.toml");
        std::fs::write(&spec, "# no segments\n").unwrap();
        assert!(load_segments(&spec).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── worker roster (`--workers-spec`) ──

    /// Write `toml` to a fresh tempfile and return its path (caller cleans up).
    fn workers_spec_file(toml: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("pb-workers-{}.toml", uuid::Uuid::now_v7().simple()));
        std::fs::write(&p, toml).unwrap();
        p
    }

    #[test]
    fn load_workers_spec_parses_roster() {
        let spec = workers_spec_file(
            "[[worker]]\nagent = \"claude\"\nmodel = \"anthropic/claude-opus-4-8\"\n\n\
             [[worker]]\nagent = \"opencode\"\nmodel = \"zai-coding-plan/glm-5.2\"\ntemperature = 0.7\n",
        );
        let roster = load_workers_spec(&spec).unwrap();
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].agent.as_deref(), Some("claude"));
        assert_eq!(
            roster[0].model.as_deref(),
            Some("anthropic/claude-opus-4-8")
        );
        assert_eq!(roster[0].temperature, None);
        assert_eq!(roster[1].agent.as_deref(), Some("opencode"));
        assert_eq!(roster[1].model.as_deref(), Some("zai-coding-plan/glm-5.2"));
        assert_eq!(roster[1].temperature, Some(0.7));
        std::fs::remove_file(&spec).ok();
    }

    #[test]
    fn load_workers_spec_rejects_empty() {
        let spec = workers_spec_file("# no workers\n");
        assert!(load_workers_spec(&spec).is_err());
        std::fs::remove_file(&spec).ok();
    }

    #[test]
    fn load_workers_spec_rejects_unknown_field() {
        // A typo'd key must be a parse error (deny_unknown_fields), not ignored.
        let spec = workers_spec_file("[[worker]]\nagent = \"claude\"\nmodle = \"oops\"\n");
        assert!(load_workers_spec(&spec).is_err());
        std::fs::remove_file(&spec).ok();
    }

    /// A minimal `DispatchOpts` for the pure `resolve_worker` test — only the
    /// roster + run-level scalar fallbacks matter; the rest are inert.
    fn opts_for_resolve(
        agent: Option<&str>,
        model: Option<&str>,
        temperature: Option<f64>,
        workers_spec: Option<Vec<WorkerSpec>>,
    ) -> DispatchOpts {
        DispatchOpts {
            from_bookmark: "base".into(),
            workers: 1,
            cmd: Some("true".into()),
            rubric: None,
            retries: 0,
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            temperature,
            workers_spec,
            memory: false,
            ttl: None,
            segments: None,
            prompt: vec![],
            json: false,
            critic: None,
            critic_policy: "record".into(),
            critic_model: "jev-latest".into(),
        }
    }

    fn worker_spec(
        agent: Option<&str>,
        model: Option<&str>,
        temperature: Option<f64>,
    ) -> WorkerSpec {
        WorkerSpec {
            agent: agent.map(str::to_string),
            model: model.map(str::to_string),
            temperature,
        }
    }

    #[test]
    fn resolve_worker_roster_wins_else_falls_back_to_run_level() {
        let roster = vec![
            // row 0 sets agent + model, leaves temperature unset → falls back.
            worker_spec(Some("claude"), Some("anthropic/claude-opus-4-8"), None),
            // row 1 sets only temperature → agent/model fall back to run-level.
            worker_spec(None, None, Some(0.9)),
        ];
        let opts = opts_for_resolve(Some("opencode"), Some("run/model"), Some(0.2), Some(roster));

        // Worker 0: roster agent/model win; temperature falls back to run-level.
        let (a0, m0, t0) = resolve_worker(&opts, 0);
        assert_eq!(a0.as_deref(), Some("claude"));
        assert_eq!(m0.as_deref(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(t0, Some(0.2));

        // Worker 1: agent/model fall back to run-level; roster temperature wins.
        let (a1, m1, t1) = resolve_worker(&opts, 1);
        assert_eq!(a1.as_deref(), Some("opencode"));
        assert_eq!(m1.as_deref(), Some("run/model"));
        assert_eq!(t1, Some(0.9));

        // No roster → every worker gets the run-level scalars (homogeneous path).
        let plain = opts_for_resolve(Some("opencode"), Some("run/model"), Some(0.2), None);
        assert_eq!(
            resolve_worker(&plain, 0),
            (
                Some("opencode".to_string()),
                Some("run/model".to_string()),
                Some(0.2)
            )
        );
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
            critic: None,
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
        // fork-`k` worker → `segments` is omitted (additive field), so existing
        // consumers see byte-identical output.
        assert!(w0.get("segments").is_none());
    }

    #[test]
    fn verdict_json_nulls_are_explicit() {
        let v = DispatchVerdict {
            winner: None,
            workers: vec![],
            pulled_to: None,
            selection_rationale: None,
            critic: None,
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
                critic: None,
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
