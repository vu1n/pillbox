//! `pillbox eval` — the declarative, reproducible eval runner (#71).
//!
//! Harness improvement is only credible if a tweak can be *measured under rerun
//! variance*, not eyeballed on one run. `eval` makes that first-class: a TOML
//! task spec declares the workspace, prompt, verifier, and a set of **variants**
//! (config A/B — different agent / model / temperature / memory / MCP); `eval`
//! runs each variant `trials` times, grades every run with the same verifier,
//! and emits a machine-readable comparison.
//!
//! **Built on the existing primitives, not a rebuild.** Each cell is the same
//! `run --detach --json` → `session send` → `session wait-idle` → `session
//! score` chain dispatch and the bash eval rig (`scripts/eval/*`) already use —
//! driven in-process through the typed session calls (`commands::run::start`,
//! `session::send`, `stream::wait_idle`, `session::score`). The genuinely-new bits are the **declarative spec** and the
//! **variant × trial matrix**. The variance statistics are NOT reimplemented:
//! `eval` emits JSONL in the exact `{task, cond, trial, score, cost}` schema
//! `scripts/eval/paired-stats.py` consumes, so the σ̂ / paired-lift CI is one
//! command away (`paired-stats.py --baseline A --treatment B <records>`).
//!
//! Spec format is **TOML** (matches `pillbox.toml`; the #71 sketch was YAML, but
//! TOML avoids a new dep and is the repo convention). Grading is exactly
//! dispatch's ([`crate::commands::grading`]): each run's live workspace is
//! untrusted, so it is frozen, protected paths (tests, fixtures) are restored from
//! a trusted base — the spec's bookmark, else a copy of the workspace directory
//! taken before the first cell — and the verifier runs only inside the grader
//! microVM. A build without the grader VM refuses to start.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::agents::{McpAttachment, RunOpts};
use crate::commands::grading::{
    copy_base_dir, dispatch_clone_source, grade_protect_set, grade_worker, grader_vm_available,
    restore_bookmark, GradeKnobs, GradePolicy, KnobNames, LiveSessions, SessionOps,
};
use crate::commands::session::{self, stream};
use crate::commands::{self};
use crate::contract::Scored;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::workspace::protect::{force_remove, ProtectSet};

/// Default per-turn idle cap (seconds) — generous; an agent turn runs minutes.
/// The monolithic-truncation lesson (docs/optimization-gate.md): too small a cap
/// cuts a long run off and inflates failure, so default high.
const DEFAULT_MAX_SECONDS: u64 = 1800;

// ── the spec (TOML) ──────────────────────────────────────────────────────────

/// A declarative eval task. Deserialized from a TOML file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalSpec {
    /// Task name — the `task` field of the emitted records (the paired-stats
    /// replication unit).
    name: String,
    /// Host dir to mount as the workspace (default cwd). Mutually exclusive with
    /// `from_bookmark`.
    #[serde(default)]
    workspace: Option<String>,
    /// Snapshot bookmark to start each run from instead of a workspace dir.
    #[serde(default)]
    from_bookmark: Option<String>,
    /// Default agent for every variant (a variant may override).
    #[serde(default)]
    agent: Option<String>,
    /// Inline prompt driven as the run's first turn. Mutually exclusive with
    /// `prompt_file`; exactly one is required.
    #[serde(default)]
    prompt: Option<String>,
    /// Read the prompt from this file instead of inline.
    #[serde(default)]
    prompt_file: Option<PathBuf>,
    /// Runs per variant — the variance sample size. Default 1.
    #[serde(default = "default_trials")]
    trials: u32,
    /// The verifier (one `cmd` xor a `rubric` file) every run is graded with.
    verify: Verify,
    /// Resource budget. Only `max_seconds` is enforced in v1 (as the per-turn
    /// idle cap); `max_turns` is accepted for forward-compat but not yet enforced.
    #[serde(default)]
    budget: Budget,
    /// Configs to compare. Empty → a single implicit `default` variant.
    #[serde(default)]
    variants: Vec<Variant>,
}

fn default_trials() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Verify {
    /// One verifier command (`sh -c`, exit 0 → pass). Xor `rubric`.
    #[serde(default)]
    cmd: Option<String>,
    /// A rubric file (`NAME :: COMMAND` per line) → per-criterion + fractional. Xor `cmd`.
    #[serde(default)]
    rubric: Option<PathBuf>,
    /// Hosts the grader microVM may reach (dispatch's `--grader-egress`). Empty
    /// keeps grading offline.
    #[serde(default)]
    grader_egress: Vec<String>,
    /// Extra protected path patterns restored from the base before every grade
    /// (dispatch's `--protect`).
    #[serde(default)]
    protect: Vec<String>,
    /// Drop the default protected set (dispatch's `--no-default-protect`).
    #[serde(default)]
    no_default_protect: bool,
    /// Hidden grader files copied into every grade tree at `.pillbox-grader/`,
    /// never into a cell's workspace (dispatch's `--grader-files`).
    #[serde(default)]
    grader_files: Option<PathBuf>,
}

impl Verify {
    /// The (cmd, rubric) pair a score request takes.
    fn parts(&self) -> Result<(Option<&str>, Option<&Path>)> {
        match (&self.cmd, &self.rubric) {
            (Some(c), _) => Ok((Some(c.as_str()), None)),
            (_, Some(r)) => Ok((None, Some(r.as_path()))),
            _ => bail!("no verifier (validate() should have caught this)"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    #[serde(default)]
    max_seconds: Option<u64>,
    /// Accepted for forward-compat; pillbox has no per-session turn cap today, so
    /// v1 does not enforce it (the agent's own scaffold bounds turns). Parsed —
    /// not yet read — so a spec can carry it without `deny_unknown_fields`
    /// rejecting the run.
    #[serde(default)]
    #[allow(dead_code)]
    max_turns: Option<u32>,
}

/// One config under comparison — the `cond` of the emitted records.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Variant {
    name: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    memory: bool,
    /// Shared MCP servers to attach, each `NAME=URL` (forwarded to `run --mcp`).
    #[serde(default)]
    mcp: Vec<String>,
}

impl EvalSpec {
    /// Validate the cross-field invariants TOML's type-check can't: exactly one
    /// verifier, exactly one prompt source, a positive trial count, not both
    /// workspace + bookmark, and the `[verify]` grading knobs exactly as dispatch
    /// validates its flags. Returns the protected set every grade restores. Loud +
    /// specific so a bad spec fails at parse, not mid-run after booting VMs.
    fn validate(&self, resolved: &Pillbox) -> Result<ProtectSet> {
        let err = |m: &str| PillboxError::usage("eval", m).into();
        if self.verify.cmd.is_some() == self.verify.rubric.is_some() {
            return Err(err("[verify] needs exactly one of `cmd` or `rubric`"));
        }
        if self.prompt.is_some() == self.prompt_file.is_some() {
            return Err(err("spec needs exactly one of `prompt` or `prompt_file`"));
        }
        if self.workspace.is_some() && self.from_bookmark.is_some() {
            return Err(err(
                "spec sets both `workspace` and `from_bookmark` — pick one",
            ));
        }
        if self.trials < 1 {
            return Err(err("`trials` must be ≥ 1"));
        }
        grade_protect_set(
            &eval_clone_source(resolved, self)?,
            &GradeKnobs {
                protect: &self.verify.protect,
                no_default_protect: self.verify.no_default_protect,
                grader_files: self.verify.grader_files.as_deref(),
            },
            &KnobNames {
                command: "eval",
                protect: "[verify] protect",
                grader_files: "[verify] grader_files",
                protect_example: "protect = [\"ci/**\"] under [verify]",
            },
        )
    }

    /// The prompt text (inline or read from `prompt_file`).
    fn prompt_text(&self) -> Result<String> {
        match (&self.prompt, &self.prompt_file) {
            (Some(p), _) => Ok(p.clone()),
            (_, Some(f)) => std::fs::read_to_string(f)
                .with_context(|| format!("read prompt_file {}", f.display())),
            _ => bail!("no prompt (validate() should have caught this)"),
        }
    }

    /// The variants to run — the declared set, or one implicit `default`.
    fn resolved_variants(&self) -> Vec<Variant> {
        if self.variants.is_empty() {
            vec![Variant {
                name: "default".into(),
                agent: None,
                model: None,
                temperature: None,
                memory: false,
                mcp: Vec::new(),
            }]
        } else {
            self.variants.clone()
        }
    }

    fn max_seconds(&self) -> u64 {
        self.budget.max_seconds.unwrap_or(DEFAULT_MAX_SECONDS)
    }
}

// ── options + per-cell result ────────────────────────────────────────────────

pub(crate) struct EvalOpts {
    pub(crate) spec: PathBuf,
    /// Emit the comparison summary as JSON on stdout instead of the human table.
    pub(crate) json: bool,
    /// JSONL records path (the paired-stats input). Default: a tempfile, printed.
    pub(crate) out: Option<PathBuf>,
}

/// One (variant, trial) outcome.
struct Cell {
    variant: String,
    trial: u32,
    /// The run's session id, or empty if the run never started.
    session: String,
    score: f64,
    passed: bool,
    /// The verifier feedback (or the error that aborted the cell) — the debug
    /// context the AC asks be surfaced loudly on failure.
    feedback: String,
    /// Set when the cell errored before producing a grade (run/drive/score
    /// failure) — distinct from a graded failure (`passed == false`).
    errored: bool,
}

// ── handler ────────────────────────────────────────────────────────────────

/// Run an eval spec: every variant × `trials`, graded with the same verifier,
/// → a machine-readable comparison + a paired-stats-ready JSONL records file.
pub(crate) fn eval(resolved: &Pillbox, opts: EvalOpts) -> Result<()> {
    let raw = std::fs::read_to_string(&opts.spec).map_err(|e| {
        PillboxError::usage("eval", format!("read spec {}: {e}", opts.spec.display()))
    })?;
    let spec: EvalSpec = toml::from_str(&raw).map_err(|e| {
        PillboxError::usage("eval", format!("parse spec {}: {e}", opts.spec.display()))
    })?;
    let protect = spec.validate(resolved)?;
    let prompt = spec.prompt_text()?;
    let variants = spec.resolved_variants();
    // Cell grades run in the grader microVM. Refuse before starting any cell
    // rather than grade a worker's tree on the host.
    if !grader_vm_available() {
        return Err(PillboxError::usage(
            "eval",
            "eval grades every cell inside a libkrun microVM, and this build has no libkrun backend",
        )
        .with_next("use a pillbox built with the default `libkrun` feature")
        .into());
    }
    let (_base_dir, base) = eval_base(resolved, &spec)?;
    let runner = Runner::new(resolved, &LiveSessions, &base, protect)?;

    let records_path = opts
        .out
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("pillbox-eval-{}.jsonl", spec.name)));
    let mut records = String::new();
    let mut cells: Vec<Cell> = Vec::new();

    for v in &variants {
        for trial in 1..=spec.trials {
            eprintln!(
                "▶ eval {} / {} trial {trial}/{}",
                spec.name, v.name, spec.trials
            );
            let cell = runner.run_cell(&spec, v, &prompt, trial);
            // The records line — paired-stats.py reads task/cond/trial/score/cost;
            // `session` is the extra log reference the AC asks for (ignored by
            // paired-stats). cost is 0 in v1 (usage capture is an additive
            // follow-up; it's off the σ̂/score metric).
            records.push_str(&format!(
                "{}\n",
                json!({
                    "task": spec.name, "cond": cell.variant, "trial": cell.trial,
                    "score": cell.score, "cost": 0.0, "session": cell.session,
                })
            ));
            if cell.errored {
                eprintln!("  ! cell errored: {}", first_line(&cell.feedback));
            } else if !cell.passed {
                // A failed verifier surfaces its output loudly (the debug context).
                eprintln!("  ✗ verifier failed: {}", first_line(&cell.feedback));
            }
            cells.push(cell);
        }
    }

    std::fs::write(&records_path, &records)
        .with_context(|| format!("write records {}", records_path.display()))?;

    let summary = summarize(&spec.name, spec.trials, &records_path, &variants, &cells);
    if opts.json {
        println!("{}", crate::paths::json_v1(vec![("eval", summary)]));
    } else {
        print_summary(&spec.name, &records_path, &variants, &cells);
    }
    Ok(())
}

/// Per-variant aggregate (the machine-readable comparison body). Pure over the
/// collected cells so the summary shape is unit-testable without a live run.
fn summarize(
    name: &str,
    trials: u32,
    records: &Path,
    variants: &[Variant],
    cells: &[Cell],
) -> serde_json::Value {
    let per: Vec<serde_json::Value> = variants
        .iter()
        .map(|v| {
            let scores: Vec<f64> = cells
                .iter()
                .filter(|c| c.variant == v.name)
                .map(|c| c.score)
                .collect();
            let n = scores.len();
            let passed = cells
                .iter()
                .filter(|c| c.variant == v.name && c.passed)
                .count();
            let mean = if n > 0 {
                scores.iter().sum::<f64>() / n as f64
            } else {
                0.0
            };
            json!({
                "name": v.name,
                "trials": n,
                "passed": passed,
                "pass_rate": if n > 0 { passed as f64 / n as f64 } else { 0.0 },
                "mean_score": mean,
                "scores": scores,
            })
        })
        .collect();
    json!({
        "name": name,
        "trials": trials,
        "records": records.to_string_lossy(),
        "variants": per,
    })
}

fn print_summary(name: &str, records: &Path, variants: &[Variant], cells: &[Cell]) {
    println!("pillbox: eval `{name}` — {} variant(s)", variants.len());
    for v in variants {
        let scores: Vec<f64> = cells
            .iter()
            .filter(|c| c.variant == v.name)
            .map(|c| c.score)
            .collect();
        let n = scores.len();
        let passed = cells
            .iter()
            .filter(|c| c.variant == v.name && c.passed)
            .count();
        let mean = if n > 0 {
            scores.iter().sum::<f64>() / n as f64
        } else {
            0.0
        };
        println!("  {:16} mean={mean:.2}  pass={passed}/{n}", v.name);
    }
    println!("  records: {}", records.display());
    if variants.len() >= 2 {
        println!(
            "  variance CI: python3 scripts/eval/paired-stats.py --baseline {} --treatment {} {}",
            variants[0].name,
            variants[1].name,
            records.display()
        );
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

// ── the live runner (typed in-process session calls) ────────────────────────

struct Runner<'a> {
    resolved: &'a Pillbox,
    /// Workspace lookup and scoring — the session API outside tests.
    ops: &'a dyn SessionOps,
    /// The trusted tree protected paths are restored from in every grade.
    base: &'a Path,
    protect: ProtectSet,
    /// Holds each cell's frozen snapshot while it is graded.
    scratch: tempfile::TempDir,
}

/// The directory `run::start` clones each cell from: the spec's workspace, else
/// the project directory, else the cwd. Validation and cloning both use it. A
/// bookmark spec forks from the bookmark, so it keeps dispatch's project rule.
fn eval_clone_source(resolved: &Pillbox, spec: &EvalSpec) -> Result<PathBuf> {
    if spec.from_bookmark.is_some() {
        return Ok(dispatch_clone_source(resolved));
    }
    let mut opts = RunOpts {
        workspace: spec.workspace.as_ref().map(PathBuf::from),
        ..RunOpts::default()
    };
    commands::run::apply_default_workspace(resolved, &mut opts);
    match opts.workspace {
        Some(dir) => Ok(dir),
        None => std::env::current_dir().context("resolve current working directory"),
    }
}

/// The trusted base every cell's protected paths are restored from, and the temp
/// dir that holds it: the spec's bookmark restored, or else a copy of the
/// workspace directory the cells are cloned from, taken before any cell runs.
fn eval_base(resolved: &Pillbox, spec: &EvalSpec) -> Result<(tempfile::TempDir, PathBuf)> {
    const PURPOSE: &str = "eval's protected-path base";
    if let Some(bookmark) = &spec.from_bookmark {
        let dir = restore_bookmark(resolved, bookmark, PURPOSE)?;
        let path = dir.path().to_path_buf();
        return Ok((dir, path));
    }
    let src = eval_clone_source(resolved, spec)?;
    let dir = copy_base_dir(&src, PURPOSE)?;
    let path = dir.path().join("ws");
    Ok((dir, path))
}

/// The `run --detach` options a (spec, variant) pair asks for — the same
/// RunOpts the CLI's `run` flags build, so an eval cell starts exactly what the
/// equivalent `pillbox run --detach` would.
fn run_opts(spec: &EvalSpec, v: &Variant) -> Result<RunOpts> {
    Ok(RunOpts {
        workspace: spec.workspace.as_ref().map(PathBuf::from),
        memory: v.memory,
        mcps: v
            .mcp
            .iter()
            .map(|m| McpAttachment::parse(m))
            .collect::<Result<_>>()?,
        detach: true,
        from_bookmark: spec.from_bookmark.clone(), // validate() rejects workspace + bookmark together
        model: v.model.clone(),
        temperature: v.temperature,
        // memory_briefed: commands::run::start fills it from the kypp briefing.
        ..RunOpts::default()
    })
}

/// The agent a cell runs: the variant's, else the spec default.
fn cell_agent<'s>(spec: &'s EvalSpec, v: &'s Variant) -> Option<&'s str> {
    v.agent.as_deref().or(spec.agent.as_deref())
}

impl<'a> Runner<'a> {
    fn new(
        resolved: &'a Pillbox,
        ops: &'a dyn SessionOps,
        base: &'a Path,
        protect: ProtectSet,
    ) -> Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("pillbox-eval-graded-")
            .tempdir()
            .context("temp dir for graded snapshots")?;
        Ok(Self {
            resolved,
            ops,
            base,
            protect,
            scratch,
        })
    }

    /// Run one (variant, trial): start a session, drive the prompt, grade its
    /// live workspace. Any error becomes an `errored` cell (score 0)
    /// — one bad cell never aborts the matrix (the other cells still run).
    fn run_cell(&self, spec: &EvalSpec, v: &Variant, prompt: &str, trial: u32) -> Cell {
        match self.run_cell_inner(spec, v, prompt) {
            Ok((session, grade)) => Cell {
                variant: v.name.clone(),
                trial,
                session,
                score: grade.score,
                passed: grade.passed,
                feedback: grade.feedback,
                errored: false,
            },
            Err(e) => Cell {
                variant: v.name.clone(),
                trial,
                session: String::new(),
                score: 0.0,
                passed: false,
                feedback: format!("{e:#}"),
                errored: true,
            },
        }
    }

    fn run_cell_inner(
        &self,
        spec: &EvalSpec,
        v: &Variant,
        prompt: &str,
    ) -> Result<(String, Scored)> {
        let session = self.start_session(spec, v)?;
        // Drive the prompt as turn 1, then block until idle (bounded by budget).
        session::send(self.resolved, &session, prompt)?;
        stream::wait_idle(
            self.resolved,
            &session,
            Some(Duration::from_secs(spec.max_seconds())),
            None,
        )?;
        let grade = self.grade(&session, spec)?;
        Ok((session, grade))
    }

    /// Start a detached session with the variant's config → the new session id.
    fn start_session(&self, spec: &EvalSpec, v: &Variant) -> Result<String> {
        let started = commands::run::start(self.resolved, cell_agent(spec, v), run_opts(spec, v)?)?;
        Ok(started.into_live_session()?.id)
    }

    /// Grade the run exactly as dispatch grades a worker: a frozen copy of its
    /// live workspace with protected paths restored from the trusted base (plus
    /// any `[verify]` grader files), scored inside the grader microVM, offline
    /// unless `[verify] grader_egress` names hosts. The snapshot is dropped afterwards.
    fn grade(&self, id: &str, spec: &EvalSpec) -> Result<Scored> {
        let (cmd, rubric) = spec.verify.parts()?;
        let policy = GradePolicy {
            base: self.base,
            protect: &self.protect,
            grader_files: spec.verify.grader_files.as_deref(),
            egress: &spec.verify.grader_egress,
            command: "eval",
        };
        let snap = self.scratch.path().join(id);
        let graded = grade_worker(self.ops, self.resolved, id, &snap, cmd, rubric, &policy);
        if snap.exists() {
            force_remove(&snap)?;
        }
        graded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<EvalSpec> {
        parse_with_protect(s).map(|(spec, _)| spec)
    }

    fn parse_with_protect(s: &str) -> Result<(EvalSpec, ProtectSet)> {
        let spec: EvalSpec = toml::from_str(s)?;
        let protect = spec.validate(&crate::pillbox::global())?;
        Ok((spec, protect))
    }

    #[test]
    fn run_opts_mirror_the_cli_flags_a_variant_implies() {
        let spec = parse(
            r#"
            name = "t"
            prompt = "p"
            from_bookmark = "base"
            [verify]
            cmd = "true"
            [[variants]]
            name = "a"
            model = "p/m"
            temperature = 0.0
            memory = true
            mcp = ["docs=https://x.example/mcp"]
            "#,
        )
        .unwrap();
        let o = run_opts(&spec, &spec.resolved_variants()[0]).unwrap();
        assert!(o.detach && o.memory && !o.vault);
        assert_eq!(o.from_bookmark.as_deref(), Some("base"));
        assert!(o.workspace.is_none());
        assert_eq!(o.model.as_deref(), Some("p/m"));
        assert_eq!(o.temperature, Some(0.0));
        assert_eq!(o.mcps.len(), 1);
        assert!(o.args.is_empty());
    }

    /// The `pillbox run …` argv eval built for a cell before it called
    /// `run::start` in-process (base 0d2e275 `start_session`), verbatim.
    fn old_cell_argv(spec: &EvalSpec, v: &Variant) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "pillbox".into(),
            "run".into(),
            "--detach".into(),
            "--json".into(),
        ];
        match (&spec.workspace, &spec.from_bookmark) {
            (Some(w), _) => args.extend(["--workspace".into(), w.clone()]),
            (_, Some(b)) => args.extend(["--from-bookmark".into(), b.clone()]),
            _ => {}
        }
        if let Some(a) = v.agent.as_ref().or(spec.agent.as_ref()) {
            args.extend(["--agent".into(), a.clone()]);
        }
        if let Some(m) = &v.model {
            args.extend(["--model".into(), m.clone()]);
        }
        if let Some(t) = v.temperature {
            args.extend(["--temperature".into(), t.to_string()]);
        }
        if v.memory {
            args.push("--memory".into());
        }
        for m in &v.mcp {
            args.extend(["--mcp".into(), m.clone()]);
        }
        args
    }

    #[test]
    fn a_cell_starts_exactly_what_the_old_cli_argv_asked_for() {
        let bookmark = parse(
            r#"
            name = "t"
            prompt = "p"
            from_bookmark = "base"
            agent = "claude"
            [verify]
            cmd = "true"
            [[variants]]
            name = "a"
            agent = "opencode"
            model = "p/m"
            temperature = 0.7
            memory = true
            mcp = ["docs=https://x.example/mcp", "search=http://example.com:8123"]
            [[variants]]
            name = "b"
            "#,
        )
        .unwrap();
        let workspace =
            parse("name = \"t\"\nprompt = \"p\"\nworkspace = \"/w\"\n[verify]\ncmd = \"true\"\n")
                .unwrap();
        for spec in [&bookmark, &workspace] {
            for v in spec.resolved_variants() {
                let argv = old_cell_argv(spec, &v);
                let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
                let (agent, opts) = crate::parse_run_argv(&argv);
                assert_eq!(cell_agent(spec, &v), agent.as_deref(), "{argv:?}");
                assert_eq!(run_opts(spec, &v).unwrap(), opts, "{argv:?}");
            }
        }
    }

    #[test]
    fn run_opts_carries_the_workspace_and_rejects_a_bad_mcp() {
        let spec =
            parse("name = \"t\"\nprompt = \"p\"\nworkspace = \"/w\"\n[verify]\ncmd = \"true\"\n")
                .unwrap();
        let v = spec.resolved_variants()[0].clone();
        let o = run_opts(&spec, &v).unwrap();
        assert_eq!(o.workspace, Some(PathBuf::from("/w")));
        let bad = Variant {
            mcp: vec!["nourl".into()],
            ..v
        };
        assert!(run_opts(&spec, &bad).is_err());
    }

    #[test]
    fn minimal_spec_parses_with_an_implicit_default_variant() {
        let spec = parse(
            r#"
            name = "smoke"
            prompt = "implement add()"
            [verify]
            cmd = "cargo test add"
            "#,
        )
        .unwrap();
        assert_eq!(spec.trials, 1);
        assert_eq!(spec.max_seconds(), DEFAULT_MAX_SECONDS);
        let vs = spec.resolved_variants();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].name, "default");
    }

    #[test]
    fn variants_and_budget_parse() {
        let spec = parse(
            r#"
            name = "ab"
            from_bookmark = "main"
            agent = "opencode"
            prompt = "do the thing"
            trials = 5
            [verify]
            rubric = "r.txt"
            [budget]
            max_seconds = 600
            [[variants]]
            name = "baseline"
            [[variants]]
            name = "memory"
            memory = true
            model = "zai/glm"
            temperature = 0.0
            mcp = ["code-search=http://localhost:8123"]
            "#,
        )
        .unwrap();
        assert_eq!(spec.trials, 5);
        assert_eq!(spec.max_seconds(), 600);
        let vs = spec.resolved_variants();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[1].name, "memory");
        assert!(vs[1].memory);
        assert_eq!(vs[1].mcp, vec!["code-search=http://localhost:8123"]);
    }

    #[test]
    fn validate_rejects_zero_and_two_verifiers() {
        // Neither verifier.
        assert!(parse("name='x'\nprompt='p'\n[verify]\n").is_err());
        // Both verifiers.
        assert!(parse("name='x'\nprompt='p'\n[verify]\ncmd='c'\nrubric='r'\n").is_err());
    }

    #[test]
    fn validate_rejects_zero_and_two_prompt_sources() {
        // No prompt.
        assert!(parse("name='x'\n[verify]\ncmd='c'\n").is_err());
        // Both prompt + prompt_file.
        assert!(parse("name='x'\nprompt='p'\nprompt_file='f'\n[verify]\ncmd='c'\n").is_err());
    }

    #[test]
    fn validate_rejects_workspace_and_bookmark_together() {
        assert!(parse(
            "name='x'\nworkspace='.'\nfrom_bookmark='m'\nprompt='p'\n[verify]\ncmd='c'\n"
        )
        .is_err());
    }

    fn cell(variant: &str, trial: u32, score: f64, passed: bool) -> Cell {
        Cell {
            variant: variant.into(),
            trial,
            session: format!("s{trial}"),
            score,
            passed,
            feedback: String::new(),
            errored: false,
        }
    }

    #[test]
    fn summarize_aggregates_per_variant() {
        let variants = vec![
            Variant {
                name: "a".into(),
                agent: None,
                model: None,
                temperature: None,
                memory: false,
                mcp: vec![],
            },
            Variant {
                name: "b".into(),
                agent: None,
                model: None,
                temperature: None,
                memory: false,
                mcp: vec![],
            },
        ];
        let cells = vec![
            cell("a", 1, 1.0, true),
            cell("a", 2, 0.0, false),
            cell("b", 1, 1.0, true),
            cell("b", 2, 1.0, true),
        ];
        let s = summarize("t", 2, Path::new("/tmp/r.jsonl"), &variants, &cells);
        assert_eq!(s["name"], "t");
        assert_eq!(s["variants"][0]["name"], "a");
        assert_eq!(s["variants"][0]["pass_rate"], 0.5);
        assert_eq!(s["variants"][0]["mean_score"], 0.5);
        assert_eq!(s["variants"][1]["pass_rate"], 1.0);
        assert_eq!(s["variants"][1]["mean_score"], 1.0);
        assert_eq!(s["variants"][1]["scores"].as_array().unwrap().len(), 2);
    }

    // ── grading: a frozen copy, protected paths restored, grader VM only ──

    use crate::commands::grading::testing::{project, put, read, FakeSessions, Project, Vm};

    /// Grade cell `c1` of a spec whose verifier is `cmd`, with `fake` standing in
    /// for the session API and `p.base` as the trusted base.
    fn grade_cell(p: &Project, fake: &FakeSessions, cmd: &str) -> Result<Scored> {
        grade_cell_with(p, fake, cmd, "")
    }

    /// [`grade_cell`] with extra `[verify]` keys (TOML lines) in the spec.
    fn grade_cell_with(
        p: &Project,
        fake: &FakeSessions,
        cmd: &str,
        verify: &str,
    ) -> Result<Scored> {
        let resolved = crate::pillbox::global();
        let (spec, protect) = parse_with_protect(&format!(
            "name='t'\nprompt='p'\n[verify]\ncmd={cmd:?}\n{verify}"
        ))
        .unwrap();
        let runner = Runner::new(&resolved, fake, p.base.path(), protect).unwrap();
        runner.grade("c1", &spec)
    }

    #[test]
    fn verify_grader_egress_reaches_the_score_request() {
        let p = project();
        let fake = FakeSessions::new(&p.ws, Vm::RecordsOnly);
        grade_cell_with(
            &p,
            &fake,
            "cargo test",
            "grader_egress = ['index.crates.io', 'static.crates.io']\n",
        )
        .unwrap();
        let scores = fake.scores();
        assert_eq!(scores[0].egress, ["index.crates.io", "static.crates.io"]);
        assert!(scores[0].in_sandbox);
    }

    #[test]
    fn verify_protect_restores_an_extra_pattern_in_the_grade_tree() {
        let p = project();
        put(p.base.path(), "ci/gate.sh", "exit 1\n");
        let ws_gate = p.ws.join("ci/gate.sh");
        put(&p.ws, "ci/gate.sh", "exit 0\n");
        let fake = FakeSessions::new(&p.ws, Vm::RecordsOnly);
        grade_cell_with(&p, &fake, "sh ci/gate.sh", "protect = ['ci/**']\n").unwrap();
        assert_eq!(read(&fake.seen.join("ci/gate.sh")), "exit 1\n");
        assert_eq!(read(&ws_gate), "exit 0\n", "live tree untouched");

        // Without the pattern the cell's own copy is graded.
        let fake = FakeSessions::new(&p.ws, Vm::RecordsOnly);
        grade_cell(&p, &fake, "sh ci/gate.sh").unwrap();
        assert_eq!(read(&fake.seen.join("ci/gate.sh")), "exit 0\n");
    }

    #[test]
    fn verify_no_default_protect_grades_the_cells_own_tests() {
        let p = project();
        put(&p.ws, "tests/check.sh", "exit 0\n");
        let fake = FakeSessions::new(&p.ws, Vm::RunsGrader);
        assert!(
            grade_cell_with(
                &p,
                &fake,
                "sh tests/check.sh",
                "no_default_protect = true\n"
            )
            .unwrap()
            .passed
        );
        assert_eq!(read(&fake.seen.join("tests/check.sh")), "exit 0\n");
    }

    #[test]
    fn verify_grader_files_land_in_the_grade_tree_only() {
        let hidden = tempfile::tempdir().unwrap();
        put(hidden.path(), "hidden.sh", "grep -q fixed src/lib.rs\n");
        let p = project();
        put(&p.ws, "src/lib.rs", "pub fn f() {} // fixed\n");
        let fake = FakeSessions::new(&p.ws, Vm::RunsGrader);
        let verify = format!("grader_files = {:?}\n", hidden.path().display().to_string());
        assert!(
            grade_cell_with(&p, &fake, "sh .pillbox-grader/hidden.sh", &verify)
                .unwrap()
                .passed
        );
        assert_eq!(
            read(&fake.seen.join(".pillbox-grader/hidden.sh")),
            "grep -q fixed src/lib.rs\n"
        );
        assert!(
            !p.ws.join(".pillbox-grader").exists(),
            "never in the cell's workspace"
        );
    }

    #[test]
    fn bad_verify_grading_knobs_fail_spec_validation() {
        let spec = |verify: &str| format!("name='t'\nprompt='p'\n[verify]\ncmd='true'\n{verify}");
        let err = parse(&spec("protect = ['../escape']\n")).unwrap_err();
        let pe = err.downcast_ref::<PillboxError>().expect("a PillboxError");
        assert!(pe.reason.contains("[verify] protect"), "{err:#}");

        let missing = tempfile::tempdir().unwrap().path().join("nope");
        let err = parse(&spec(&format!(
            "grader_files = {:?}\n",
            missing.display().to_string()
        )))
        .unwrap_err();
        assert!(format!("{err:#}").contains("not a directory"), "{err:#}");

        // Tests run from the package root: a directory inside it would ship to every cell.
        let err = parse(&spec("grader_files = 'src'\n")).unwrap_err();
        assert!(format!("{err:#}").contains("inside the project"), "{err:#}");
    }

    #[test]
    fn grader_files_are_refused_inside_the_spec_workspace_wherever_it_is() {
        // The workspace is outside the cwd project; cells clone from it, not the cwd.
        let ws = tempfile::tempdir().unwrap();
        let inside = ws.path().join("hidden-tests");
        std::fs::create_dir(&inside).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let spec = |ws: &Path, gf: &Path| {
            format!(
                "name='t'\nprompt='p'\nworkspace = {:?}\n[verify]\ncmd='true'\ngrader_files = {:?}\n",
                ws.display().to_string(),
                gf.display().to_string()
            )
        };
        let err = parse(&spec(ws.path(), &inside)).unwrap_err();
        assert!(format!("{err:#}").contains("inside the project"), "{err:#}");
        assert!(parse(&spec(ws.path(), outside.path())).is_ok());
    }

    #[test]
    fn eval_never_grades_a_cell_tree_on_the_host() {
        let host = tempfile::tempdir().unwrap();
        let marker = host.path().join("ran-on-host");
        let touch = format!("touch '{}'\n", marker.display());
        let p = project();
        put(&p.ws, "build.sh", &touch);
        put(&p.ws, "tests/check.sh", &touch);
        let fake = FakeSessions::new(&p.ws, Vm::RecordsOnly);
        assert!(
            !grade_cell(&p, &fake, "sh build.sh && sh tests/check.sh")
                .unwrap()
                .passed
        );

        assert!(!marker.exists(), "cell code executed on the host");
        let scores = fake.scores();
        assert_eq!(scores.len(), 1, "graded exactly once: {scores:?}");
        let score = &scores[0];
        assert_eq!(score.id, "c1");
        assert!(score.in_sandbox, "{score:?}");
        assert!(
            score.drop_privileges,
            "grader must not run as root: {score:?}"
        );
        assert_eq!(score.snapshot, None, "{score:?}");
        assert!(
            score.egress.is_empty(),
            "the grader VM is offline: {score:?}"
        );
        let tree = score.workspace.as_deref().expect("graded an explicit tree");
        assert!(
            !tree.starts_with(&p.ws),
            "graded the live workspace: {score:?}"
        );
        // The VM got the cell's own code, and the base's test.
        assert!(fake.seen.join("build.sh").exists());
        assert_eq!(
            read(&fake.seen.join("tests/check.sh")),
            "grep -q fixed src/lib.rs\n"
        );
    }

    #[test]
    fn a_cell_editing_a_protected_test_does_not_change_its_grade() {
        let check = "sh tests/check.sh";

        let p = project();
        let fake = FakeSessions::new(&p.ws, Vm::RunsGrader);
        assert!(!grade_cell(&p, &fake, check).unwrap().passed);

        // Rewriting the test to pass grades as the base has it: still failing.
        let p = project();
        put(&p.ws, "tests/check.sh", "exit 0\n");
        let fake = FakeSessions::new(&p.ws, Vm::RunsGrader);
        assert!(!grade_cell(&p, &fake, check).unwrap().passed);
        assert_eq!(
            read(&fake.seen.join("tests/check.sh")),
            "grep -q fixed src/lib.rs\n"
        );
        assert_eq!(
            read(&p.ws.join("tests/check.sh")),
            "exit 0\n",
            "live tree untouched"
        );

        // A real fix outside the protected set still passes.
        let p = project();
        put(&p.ws, "src/lib.rs", "pub fn f() {} // fixed\n");
        let fake = FakeSessions::new(&p.ws, Vm::RunsGrader);
        assert!(grade_cell(&p, &fake, check).unwrap().passed);
    }

    #[test]
    fn a_grader_vm_that_cannot_run_errors_the_cell_with_no_host_retry() {
        let p = project();
        let fake = FakeSessions::new(&p.ws, Vm::Down);
        let err = format!("{:#}", grade_cell(&p, &fake, "true").unwrap_err());
        assert!(err.contains("before the grader started"), "{err}");
        let scores = fake.scores();
        assert_eq!(scores.len(), 1, "no second attempt: {scores:?}");
        assert!(scores[0].in_sandbox);
    }

    #[cfg(not(feature = "libkrun"))]
    #[test]
    fn eval_refuses_to_start_without_the_grader_vm() {
        crate::test_util::with_isolated_home("eval-no-grader-vm", || {
            let dir = tempfile::tempdir().unwrap();
            let spec = dir.path().join("spec.toml");
            std::fs::write(&spec, "name='t'\nprompt='p'\n[verify]\ncmd='true'\n").unwrap();
            let out = dir.path().join("records.jsonl");
            let err = eval(
                &crate::pillbox::global(),
                EvalOpts {
                    spec,
                    json: false,
                    out: Some(out.clone()),
                },
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("libkrun"), "{err:#}");
            assert!(!out.exists(), "no cell ran");
        });
    }
}
