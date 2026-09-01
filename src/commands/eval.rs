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
//! driven by self-exec'ing the pillbox binary (the documented `--json`
//! contracts). The genuinely-new bits are the **declarative spec** and the
//! **variant × trial matrix**. The variance statistics are NOT reimplemented:
//! `eval` emits JSONL in the exact `{task, cond, trial, score, cost}` schema
//! `scripts/eval/paired-stats.py` consumes, so the σ̂ / paired-lift CI is one
//! command away (`paired-stats.py --baseline A --treatment B <records>`).
//!
//! Spec format is **TOML** (matches `pillbox.toml`; the #71 sketch was YAML, but
//! TOML avoids a new dep and is the repo convention). Grading is libkrun-only in
//! v1 — like [`crate::commands::dispatch`], it scores each run's *live* workspace
//! clone via `session info --json` → `.session.workspace`; a docker path (pull-
//! then-score) is the same deferred resolution dispatch notes.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::contract::Scored;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;

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
    /// verifier, exactly one prompt source, a positive trial count, and not both
    /// workspace + bookmark. Loud + specific so a bad spec fails at parse, not
    /// mid-run after booting VMs.
    fn validate(&self) -> Result<()> {
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
        Ok(())
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
    spec.validate()?;
    let prompt = spec.prompt_text()?;
    let variants = spec.resolved_variants();
    let runner = Runner::new(resolved)?;

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

// ── the live runner (subprocess self-exec) ───────────────────────────────────

// NOTE: the run→drive→grade subprocess chain mirrors dispatch's `CliDriver`. A
// shared `SessionRunner` seam (factoring fork/wait-idle/grade out of both) is a
// clean behavior-preserving follow-up; kept eval-local here so #71 doesn't
// refactor just-landed dispatch on its way in.
struct Runner {
    exe: PathBuf,
}

impl Runner {
    fn new(_resolved: &Pillbox) -> Result<Self> {
        let exe = std::env::current_exe().context("locate the pillbox binary")?;
        Ok(Self { exe })
    }

    /// Run one (variant, trial): start a session, drive the prompt, grade its
    /// live workspace. Any subprocess error becomes an `errored` cell (score 0)
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
        self.status(&[
            "session".into(),
            "send".into(),
            session.clone(),
            prompt.into(),
        ])?;
        self.status(&[
            "session".into(),
            "wait-idle".into(),
            session.clone(),
            "--timeout".into(),
            spec.max_seconds().to_string(),
        ])?;
        let grade = self.grade(&session, spec)?;
        Ok((session, grade))
    }

    /// `run --detach --json` with the variant's config → the new session id.
    fn start_session(&self, spec: &EvalSpec, v: &Variant) -> Result<String> {
        let mut args = vec!["run".into(), "--detach".into(), "--json".into()];
        match (&spec.workspace, &spec.from_bookmark) {
            (Some(w), _) => args.extend(["--workspace".into(), w.clone()]),
            (_, Some(b)) => args.extend(["--from-bookmark".into(), b.clone()]),
            _ => {} // cwd default
        }
        // A variant's agent overrides the spec default.
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
        let out = self.capture(&args)?;
        let val: serde_json::Value =
            serde_json::from_str(&out).with_context(|| format!("parse `run --json`: {out:?}"))?;
        val["session"]["id"]
            .as_str()
            .map(str::to_string)
            .context("`run --json` had no session.id")
    }

    /// Grade the run's live workspace clone in place (libkrun-only, like
    /// dispatch): `session info --json` → `.session.workspace`, then `session
    /// score --workspace … --json` → the parsed verdict.
    fn grade(&self, id: &str, spec: &EvalSpec) -> Result<Scored> {
        let info = self.capture(&["session".into(), "info".into(), id.into(), "--json".into()])?;
        let iv: serde_json::Value = serde_json::from_str(&info)
            .with_context(|| format!("parse `session info`: {info:?}"))?;
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
        match (&spec.verify.cmd, &spec.verify.rubric) {
            (Some(c), _) => args.extend(["--cmd".into(), c.clone()]),
            (_, Some(r)) => args.extend(["--rubric".into(), r.to_string_lossy().into_owned()]),
            _ => bail!("no verifier (validate() should have caught this)"),
        }
        let out = self.capture(&args)?;
        // The `score --json` envelope deserializes into `Scored` (extra
        // version/session/seq keys ignored) — a wire-contract change is a compile
        // error, not a silent default.
        serde_json::from_str(&out).with_context(|| format!("parse `score --json`: {out:?}"))
    }

    /// Run a subcommand, require success, return stdout.
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

    /// Run a subcommand for its exit status only; its stdout banner is discarded
    /// so eval's own `--json` stays pure.
    fn status(&self, args: &[String]) -> Result<()> {
        self.capture(args).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<EvalSpec> {
        let spec: EvalSpec = toml::from_str(s)?;
        spec.validate()?;
        Ok(spec)
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
}
