# `pillbox eval` — the declarative, reproducible eval runner

Harness improvement is only credible if a tweak can be **measured under rerun
variance**, not eyeballed on one run (the whole σ̂ lesson — see
[optimization-gate.md](./optimization-gate.md)). `pillbox eval` makes that
first-class: a TOML task spec declares the workspace, prompt, verifier, and a set
of **variants** (config A/B); `eval` runs each variant `trials` times, grades
every run with the same verifier, and emits a machine-readable comparison plus a
**`paired-stats.py`-ready JSONL records file**.

## What it is (and isn't)

It's the spec-driven, first-class front-end over primitives pillbox already has —
each cell is the same run → send → wait-idle → grade chain `dispatch` and the
bash eval rig (`scripts/eval/*`) use, driven in-process through the typed
session calls (`commands::run::start`, `session::send`, `stream::wait_idle`,
`session::workspace_of`, `session::score`) rather than by self-exec'ing the
binary. The new bits are the **declarative spec** and the
**variant × trial matrix**.

It does **not** reimplement the variance statistics: `eval` emits records in the
exact `{task, cond, trial, score, cost}` schema
[`scripts/eval/paired-stats.py`](../scripts/eval/paired-stats.py) consumes, so
the σ̂ / paired-lift CI is one command away.

## The spec (TOML)

```toml
name = "auth-validation-smoke"   # the records' `task` field (the replication unit)
workspace = "."                  # host dir to mount; xor `from_bookmark = "main"`
agent = "opencode"               # default agent (a variant may override)
prompt = "Implement request validation in src/auth.rs"   # xor `prompt_file = "./tasks/auth.md"`
trials = 5                       # runs per variant — the variance sample size

[verify]                         # exactly one of cmd / rubric
cmd = "cargo test auth_validation"
# rubric = "./auth.rubric"       # NAME :: COMMAND per line → per-criterion + fractional score

[budget]
max_seconds = 900                # per-turn idle cap (the wait-idle timeout)
# max_turns = 20                 # accepted for forward-compat; not enforced in v1

[[variants]]                     # omit entirely → one implicit `default` variant
name = "baseline"

[[variants]]
name = "with-context-mcp"
memory = true
model = "zai-coding-plan/glm-5.1"
temperature = 0.0
mcp = ["code-search=http://localhost:8123"]   # each forwarded to `run --mcp`
```

Spec format is **TOML** (matches `pillbox.toml` and avoids a new dependency; the
#71 sketch was YAML — TOML is the repo convention). Unknown fields are rejected
loudly (`deny_unknown_fields`), and the cross-field invariants (exactly one
verifier, exactly one prompt source, not both workspace + bookmark, `trials ≥ 1`)
fail at parse — **before any VM boots**.

## Run it

```sh
pillbox eval ./auth-smoke.toml                 # human summary table + records path
pillbox eval ./auth-smoke.toml --json          # machine-readable summary on stdout
pillbox eval ./auth-smoke.toml --out runs.jsonl # pin the records path
```

The summary (`--json`):

```jsonc
{
  "version": 1,
  "eval": {
    "name": "auth-validation-smoke",
    "trials": 5,
    "records": "/tmp/pillbox-eval-auth-validation-smoke.jsonl",  // the paired-stats input
    "variants": [
      { "name": "baseline", "trials": 5, "passed": 3, "pass_rate": 0.6,
        "mean_score": 0.64, "scores": [1.0, 0.0, 1.0, 0.6, 0.6] },
      { "name": "with-context-mcp", "trials": 5, "passed": 5, "pass_rate": 1.0,
        "mean_score": 1.0, "scores": [1.0, 1.0, 1.0, 1.0, 1.0] }
    ]
  }
}
```

Each records line is `{"task","cond","trial","score","cost","session"}` —
`paired-stats.py` reads `task`/`cond`/`trial`/`score`; `session` is the §0
log reference for the run (its `scored` event + trajectory). A run that errors
before grading (boot/drive/score failure) is recorded as `score 0` and its error
is surfaced loudly to stderr — one bad cell never aborts the matrix.

## The variance CI (don't eyeball the means)

The summary's per-variant means are the headline; the **decision** needs the
paired-over-trials σ̂ + lift CI, which the records file feeds directly:

```sh
python3 scripts/eval/paired-stats.py \
  --baseline baseline --treatment with-context-mcp \
  /tmp/pillbox-eval-auth-validation-smoke.jsonl
```

→ `{sigma_hat, mean_d, ci_low, ci_high, sensitive, …}`. A lift whose CI excludes
zero is real; a mean difference smaller than σ̂ is noise. (The `eval` human
summary prints this exact command.)

## Headroom, variance, saturated families (read before trusting a result)

The hard-won lessons the σ̂ campaigns paid for (see
[optimization-gate.md](./optimization-gate.md)):

- **Headroom.** A *saturated* task family (every variant scores ~1.0) has no
  capability-variance to express — a variant can't show a delta there. Pick a
  task the baseline *bistably* fails (the room a gain lands in).
- **Variance is the enemy you're measuring.** `trials = 1` tells you nothing
  about reliability. Use enough trials that σ̂ is meaningful (the campaigns used
  n=10/cell); compare with `paired-stats.py`, not raw means.
- **Truncation inflates failure.** Too small a `[budget] max_seconds` cuts a long
  run off and looks like a capability failure. Default is generous (1800s).
- **Determinism knob.** `temperature = 0` is greedy — the variance floor. A
  variant exists to change *one* thing; hold the rest equal.

## Limits (v1)

- **Grading is dispatch's grading** (`commands::grading::grade_worker`). A run's
  workspace is written by the agent inside its microVM, so it is untrusted: eval
  freezes a copy of it (`session::workspace_of`, libkrun sessions only), restores
  the default protected paths (`tests/`, `*_test.rs`, fixtures — see
  `workspace::protect::DEFAULT_PROTECTED`) from a trusted base, and runs the
  verifier only inside the one-shot grader microVM — offline, as the unprivileged
  user. A cell that rewrites a test is graded against the base's copy; a grader VM
  that cannot run errors the cell (score 0), never a host retry. The base is the
  spec's `from_bookmark` restored, or else a scrubbed copy of the workspace
  directory (`workspace`, the project dir, or the cwd) taken before the first
  cell. A build without the libkrun backend refuses to start `eval`.
- **`cost` is 0** in the records — usage capture is an additive follow-up; it's
  off the σ̂/score metric the comparison turns on.
- **`max_turns`** is parsed but not enforced (pillbox has no per-session turn cap;
  the agent's own scaffold bounds turns).
- A live end-to-end **smoke** (mirroring `scripts/smoke/dispatch.sh`) is the
  follow-up that exercises the run loop on real VMs; the loop reuses dispatch's
  same run→drive→grade chain (now the typed in-process calls), and the spec/validation/summary
  are unit + CLI tested.

## Relationship to other verbs

- `dispatch` is `eval`'s sibling: same run→drive→score primitives, but `dispatch`
  forks *k identical* workers and **selects a winner** (best-of-k), while `eval`
  runs *different* configs and **compares** them (no selection). Use `dispatch`
  to get one good result; use `eval` to decide which config is better.
- `session score` is the reward channel both ride; `paired-stats.py` is the
  shared variance analysis.
