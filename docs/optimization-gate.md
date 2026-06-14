# Optimization-layer gate — result + decision

**Date:** 2026-06-03 · **Decision: do NOT build the DSPy/GEPA meta-harness.**

> **Update — 2026-06-14 · GHOST-007:** a second result, on a different axis, is at
> the bottom of this doc — the **σ̂-segmentation keystone**. The gate below
> falsified the *instruction* layer (no headroom); the keystone tests a
> *structural* lever (segment the long horizon) on a headroom task and is
> **positive** — segmentation collapses σ̂ (0.467 → 0.000). The two are
> complementary; see the **σ̂-segmentation keystone** section below.

The "prove it before you build it" gate from [swarm-memory.md](./swarm-memory.md) and the
deep-research verdict (memory: `pillbox-optimization-layer-verdict`): before committing to a
prompt/program optimizer (GEPA-style) on top of the agent, run the cheapest experiment that
could falsify the thesis. It falsified it.

## The question

Does a learned/curated instruction layer beat a strong base model + good prompt on *our*
task (open-ended code editing, sparse reward)? Three arms, scored on a held-out split:

- **baseline** — glm-5.1, task prompt only.
- **GEPA** — a profile distilled by reflecting on the agent's *train-split* failures
  (`meta-harness/propose.sh`), then scored on the held-out split. NB: the held-out split is a
  **validation** set, not a blind test — `optimize.sh` selects candidates on it, so across
  multiple rounds it gets tuned against. This run was `rounds=1` (one candidate, minimal
  selection pressure), but a real "generalization" claim needs a third, locked test split.
  The selection bias *flatters* the optimizer, which only sharpens the negative result below.
- **ACE** — a hand-curated playbook prepended at run time (`eval/memory/playbook.md`).

## Method

- 20 frozen [aider-polyglot](https://github.com/Aider-AI/polyglot-benchmark) Python tasks,
  content-addressed in an `evals` pillbox (`freeze-task.sh`), split 10 train / 10 held-out.
- **Graded** reward via `session score --rubric`: one criterion per test method
  (`gen-rubrics.py`), score = fraction passing. This is the load-bearing choice — binary
  pass/fail is too noisy and starves GEPA of its textual gradient; the rubric gives a
  fractional score **and** per-criterion feedback.
- Model glm-5.1 (zai-coding-plan), libkrun backend, `TRIALS=1`, `rounds=1`.
- Driver: `meta-harness/optimize.sh` (baseline + GEPA) and the ACE arm over the same
  held-out frozen tasks. Metric = **mean rubric score**.

## Result

| arm | mean | perfect |
|---|---:|---:|
| baseline (strong base, no profile) | **0.690** | 5/10 |
| GEPA (19-line distilled profile) | **0.702** | 5/10 |
| ACE (curated playbook) | **0.706** | 5/10 |

A dead heat — all three within **0.016**. GEPA's +0.012 over baseline is noise, not lift.

Per-task (held-out), the profiles **help some tasks and break others, netting to ~zero**:

| task | baseline | GEPA | ACE |
|---|---:|---:|---:|
| beer_song | 1.00 | **0.00** | 1.00 |
| bottle_song | 1.00 | 1.00 | 1.00 |
| connect | 1.00 | 1.00 | 1.00 |
| dot_dsl | 0.58 | 0.42 | 0.33 |
| forth | 0.00 | 0.00 | 0.00 |
| grade_school | 0.75 | 0.75 | 0.75 |
| hangman | 0.57 | 0.86 | 1.00 |
| paasio | 1.00 | 1.00 | **0.04** |
| pig_latin | 1.00 | 1.00 | 1.00 |
| pov | 0.00 | 1.00 | 0.93 |

GEPA *broke* beer_song (1.0→0.0) and *fixed* pov (0.0→1.0); ACE broke paasio. The single-task
swings (≈±0.5) dwarf the inter-arm mean gap (0.016) — and the *same task, same condition*
flips run to run (connect scored 0.6 then 1.0 across two runs). At `TRIALS=1` the per-task
signal is noise; only the mean is loosely interpretable, and the means tie.

## Verdict

**A strong base model + a good prompt is all of it on this task.** This matches the
deep-research prediction: glm-5.1's baseline is already 0.69, so there's no headroom for an
optimizer to fill — the regime where prompt-optimizers add least. Per the gate's own
criterion ("if GEPA's gain doesn't durably beat both baseline and ACE, the thesis is
falsified before any harness build"), it is falsified.

Do **not** build the optimizer layer for this regime. Revisit only on a task where the base
model genuinely struggles (real headroom); there an optimizer *might* pay off, here it
provably doesn't.

## What this run validated (the keepers)

The substrate, end-to-end, on its own primitives — the real deliverable:

- **Frozen content-addressed evals** — `push --bookmark` / `pull`, reproducible reruns.
- **Drive surface** — `run --json` → `session send` → `session wait-idle` → `session info`.
- **Verifiable graded reward** — `session score --rubric`: forge-resistant (per-criterion
  exit codes, base64-framed output), fractional score, per-criterion textual feedback. NOT
  self-reported (Goodhart-safe).

Build on these. The eval rig (`scripts/eval/`, `scripts/meta-harness/`) is reusable for the
next task.

## Caveats (honest, not escape hatches)

This is a **credible eval prototype, not a durable experiment artifact.** No seeds, manifests,
confidence intervals, locked final test split, or resumable run records — the result is live
runs + component checks, not a publication-grade artifact. Calibrate confidence accordingly.

Most biases tilt *toward* the optimizer, so the negative verdict is robust to them:
- 1 round, 1 task-family (aider-polyglot Python), 1 model (glm-5.1), `TRIALS=1` → strictly
  "no *detectable* lift." A `TRIALS=3` run tightens the bars, but a 0.016 gap doesn't become a
  real win.
- Held-out is a **validation** split (selection pressure flatters GEPA); a blind test split
  would only confirm or worsen the result.
- ACE's playbook was partly distilled from beer/bottle_song (both in held-out) — contamination
  tilting ACE **up**, and it still only ties.

The one caveat that could cut *toward* the optimizer:
- The trajectory fed to `propose` is thin — tool **names + final statuses** only, not inputs,
  outputs, diffs, or messages (`run-task.sh`). It also gets the per-criterion grader feedback
  (GEPA's core outcome gradient), so it isn't starved, but the "diagnose the *process*" claim
  is over-stated; a richer trajectory might give GEPA more to work with.

Engineering / positioning warts to fix before calling this serious:
- `optimize.sh`'s `reap()` uses broad `pkill -f __krun-vmm` / `pillbox run` — hostile to other
  sessions on a shared machine. Replace with per-`sid` teardown (a trap in `run-task.sh`) or
  `--ttl` + `session prune`.
- Grading defaults to the **host** (`run-task.sh`, `rubric-loop.sh`); for untrusted tasks /
  hermeticity use `session score --in-sandbox` (+ `--grader-egress` for dep fetches).
- `rubric-loop.sh` is **hidden files, visible feedback** — it injects failed-criterion names +
  feedback into the next turn (self-correction needs this), so it can't claim "the agent can't
  see the checks." A final blind grader (no feedback leak) should produce the measured score.

## Reproduce

```sh
# build + sign the libkrun binary (docs/libkrun-sandbox.md); for long batches run from an
# immune copy — target/debug can be clobbered by an external `cargo build` (rust-analyzer /
# save-hook) without the libkrun feature → docker fallback → silent `no-workspace` scores.
python3 scripts/eval/import-aider-polyglot.py --limit 20
python3 scripts/eval/gen-rubrics.py
# freeze train/held-out into an `evals` pillbox, then:
PILLBOX_BACKEND=libkrun MODEL=zai-coding-plan/glm-5.1 TRIALS=1 SET=aider \
  scripts/meta-harness/optimize.sh --rounds 1
```

---

# σ̂-segmentation keystone (GHOST-007) — 2026-06-14

**Decision: the variance frame HOLDS. Segmentation cuts σ̂ — decisively on the first
task (σ̂ 0.467 → 0.000, mean 0.42 → 1.00). Proceed; confirm across more tasks before
trusting the magnitude.**

The structural-lever follow-up the gate above invited ("revisit on a task where the
base model genuinely struggles — real headroom"). That gate falsified the
*instruction* layer (GEPA/ACE) in the no-headroom regime; this tests a *structural*
lever — cutting the long agentic horizon into rubric-gated **segments** — on exactly
such a headroom task, and it is positive. Complementary, not contradictory:
prompt-optimization doesn't help a strong base model; horizon-segmentation does help a
model that *bistably* fails a long horizon.

## The question

Holding model, task, and grader fixed: does running a task as rubric-gated
**segments** — a *fresh* session per segment over the prior segment's verified
workspace (the horizon RESET at each checkpoint) — cut the trial-to-trial variance σ̂
vs a **monolithic** single-horizon run? Harness: `scripts/eval/segmentation/`
(GHOST-006). Both arms scored on the SAME authoritative full rubric.

## Method

- 1 task — `ap_pov` (Aider-polyglot "Pov": `from_pov` reroot → `path_to`, a genuine
  sequential split), the headroom task the gate above flagged (glm-5.1 monolithic is
  bistable on it). 2 segments — 01-reroot (8 hidden tests) → 02-pathfind (7); gates =
  authoritative subsets of the hidden `pov_test.py`, injected at grade time.
- Model glm-5.1 (zai-coding-plan), libkrun, TEMPERATURE=0 (greedy), **n=10 trials/arm**,
  MAX_WAIT=600 (anti-truncation).
- Metric = full-rubric fractional score; **σ̂ = per-arm trial-to-trial SD** (in-script
  summary); `paired-stats.py` for the paired lift.
- Durable records: `scripts/eval/segmentation/results/ap_pov-glm51-n10.jsonl` (the
  resumable artifact the gate above lacked).

## Result

| arm | scores (n=10) | σ̂ | mean | perfect |
|---|---|---:|---:|---:|
| monolithic | 1, 1, 0, 0, 0, .47, 0, .8, 0, .93 | **0.467** | 0.42 | 2/10 |
| segmented | 1 ×10 | **0.000** | 1.00 | 10/10 |

**Δσ̂ = −0.467 (variance collapsed to zero) · mean lift +0.58 · perfect-rate
0.20 → 1.00.** (`paired-stats` mean_d = +0.58; its bootstrap CI is degenerate at
n_tasks = 1.)

The monolithic arm is textbook long-horizon **bistability**: 5/10 trials bail (0.0),
and — confirmed by the cost telemetry — these are *genuine minimal attempts*
(≈ $0.013, a real but short turn), **not** session failures (which emit cost 0; the
batch has zero such records). Handed the whole task at once, the agent frequently
ships broken/incomplete `pov.py`. The segmented arm never bails: each shorter, gated
segment keeps it on-task, and it converges correct every trial.

(Artifact-capture of a concrete failing `pov.py` was blocked by **post-batch
libkrun-VM exhaustion** — fresh VMs intermittently failed to boot *after* the
30-session batch while docker stayed healthy. The completed batch is unaffected
(verified: zero cost-0 records, no per-trial degradation trend); the exhaustion is
itself a scaling note for larger runs — reap VMs between batches.)

## Verdict

**Segmentation cuts σ̂ — yes, decisively, on this task.** The checkpoint-gated horizon
reset removes the long-horizon failure mode: it collapses the variance (0.467 → 0)
*and* lifts the mean (0.42 → 1.0). The variance frame holds; ghost's "fork verified
worker loops + segment long horizons" design is validated on its first keystone
measurement — on exactly the headroom task the instruction-layer gate pointed at.

## Caveats (the effect is large enough to demand them)

The magnitude (σ̂ → 0, +58 pts) far exceeds the literature's single-to-low-double-digit
gains, so on ONE task "ap_pov is unusually segmentation-friendly" stays live until
confirmed:

1. **n_tasks = 1.** ap_pov only — the paired CI is degenerate (one replication unit).
   The per-arm σ̂ comparison is valid (10 trials each); cross-task generalization is
   not. **Immediate follow-up: ≥ 3–5 segmented tasks** (decompose more of the
   aider-polyglot family into `segments/`).
2. **Co-variable — per-checkpoint retry.** The segmented arm gets `SEG_RETRIES=1`
   (≤ 1 retry per segment on a gate fail); the monolithic arm gets one shot. So the win
   blends horizon-reset + checkpoint-enabled-retry. Cost data (segmented totals
   $0.05–0.12, no retry blow-up) argues the *reset* is the driver, not extra attempts —
   but to ISOLATE it, add a `SEG_RETRIES=0` segmented arm and/or a retried monolithic
   arm.
3. **ap_pov decomposes cleanly** (reroot → pathfind is near-independent + sequential).
   Entangled tasks may segment worse — part of what the multi-task follow-up measures.
4. Verifier quality controlled: final grade = the authoritative full pov_test (15
   methods); gates are its subsets — not a weak-rubric artifact.

## Reproduce

```sh
# codesigned libkrun binary; materialize ap_pov; run from an IMMUNE binary copy
# (an external cargo build clobbers target/debug → docker fallback → silent zeros):
cp target/debug/pillbox /tmp/pb && python3 scripts/eval/import-aider-polyglot.py --limit 20
PILLBOX=/tmp/pb MODEL=zai-coding-plan/glm-5.1 TRIALS=10 PILLBOX_BACKEND=libkrun \
  PILLBOX_RUNNER_IMAGE=pillbox-runner:l7 \
  OUT=scripts/eval/segmentation/results/ap_pov-glm51-n10.jsonl \
  scripts/eval/segmentation/run.sh scripts/eval/tasks/ap_pov
```

---

# σ̂-segmentation H1 — multi-task confirmation (GHOST-007 hardening) — 2026-06-15

**Decision: the keystone GENERALIZES. Segmentation's benefit replicates across 3 tasks with
a paired lift CI that excludes zero — the n_tasks=1 caveat above is resolved. Proceed.**

The #1 caveat on the GHOST-007 verdict was n_tasks=1 (degenerate paired CI; "ap_pov may be
unusually segmentation-friendly"). H1 ran the same harness on **3** genuinely-sequential
tasks to test generalization. It generalizes — and reveals the benefit has **two distinct
mechanisms**, not just variance collapse.

## Method

- 3 tasks, each a real sequential split: `ap_dot_dsl` (build → validate), `ap_grade_school`
  (add → query), `ap_pov` (reroot → pathfind). Gates = authoritative subsets of each task's
  hidden tests.
- glm-5.1, libkrun, TEMPERATURE=0, **n=10 trials/arm**, MAX_WAIT=600 (truncation-safe).
- Records: `scripts/eval/segmentation/results/h1-3task-glm51-n10.jsonl` (60/60 clean, zero
  cost-0 / failed launches).

## Result

| task | monolithic mean·σ̂ | segmented mean·σ̂ | Δmean | mechanism |
|---|---|---|---:|---|
| `grade_school` | 0.75 · **0.00** | 1.00 · 0.00 | +0.25 | deterministic → completion lift, no variance to cut |
| `dot_dsl` | 0.16 · 0.17 | 0.69 · 0.08 | +0.53 | mildly bistable → σ̂ cut + lift |
| `pov` | 0.54 · **0.47** | 1.00 · 0.00 | +0.46 | wildly bistable → σ̂ collapsed + lift |

**Pooled per-arm σ̂: monolithic 0.212 → segmented 0.026** (perfect-rate 0/30 → 20/30).
**Paired lift `mean_d = +0.41`, bootstrap CI over tasks `[0.25, 0.53]` — excludes zero.**

Two findings:

1. **σ̂ scales with horizon complexity** (the variance frame's prediction, now visible across
   tasks): at TEMPERATURE=0, monolithic σ̂ is 0.00 (grade_school, simple) < 0.17 (dot_dsl) <
   0.47 (pov, long). pov swings 0.93↔0.0 at temp=0 — bistability *beyond* sampling noise, the
   long-horizon instability claim. Segmentation shortens each horizon → σ̂ → ~0.
2. **Segmentation helps via TWO mechanisms.** Where the monolithic horizon is *bistable*
   (pov, dot_dsl) it **cuts σ̂**. Where it's *deterministic-but-incomplete* (grade_school, a
   flat 0.75 every trial — the agent consistently under-scopes the second stage) it helps
   purely by **mean lift / completion** — the focused sub-prompt makes the remaining scope
   explicit. The σ̂-cut needs pre-existing variance to express; the mean lift is universal.

## Verdict

**The keystone generalizes.** Segmentation robustly improves outcomes across all 3 tasks (lift
CI excludes zero), cuts σ̂ wherever the monolithic horizon is bistable, and reproduces
GHOST-007's ap_pov result near-exactly (monolithic σ̂ 0.47 both runs). The variance frame
holds across tasks, with a richer understanding: decomposition buys *both* variance-collapse
(on long/bistable horizons) and scope-completion (on horizons the agent under-scopes).

## Caveats / open questions

1. **Mechanism bundling.** Segmented = horizon-reset + focused sub-prompts + per-checkpoint
   retry (SEG_RETRIES=1). The mean lift likely owes much to the *focused prompts* making scope
   explicit — which is arguably segmentation's point, not a confound. **H2 (`SEG_RETRIES=0`)
   isolates the retry contribution**; a focused-prompt-only ablation would isolate the rest.
2. **dot_dsl stays hard** — segmented caps ~0.69 (never perfect); glm-5.1 doesn't fully solve
   it either way. Segmentation helps but isn't a capability substitute.
3. Single model (glm-5.1). H5 (cross-model) remains the next external-validity step.

## Reproduce

```sh
# from an IMMUNE binary copy on a host with FREE DISK (a full disk half-launches libkrun VMs
# → stalls; see ../memory pillbox-libkrun-host-fragility). The harness reaps krun state per
# session, but the runner images/rootfs cache still need headroom.
PILLBOX=/tmp/pb MODEL=zai-coding-plan/glm-5.1 TRIALS=10 MAX_WAIT=600 PILLBOX_BACKEND=libkrun \
  PILLBOX_RUNNER_IMAGE=pillbox-runner:l7 \
  OUT=scripts/eval/segmentation/results/h1-3task-glm51-n10.jsonl \
  scripts/eval/segmentation/run.sh \
    scripts/eval/tasks/ap_dot_dsl scripts/eval/tasks/ap_grade_school scripts/eval/tasks/ap_pov
```

## H2 — retry isolation (`SEG_RETRIES=0`), 2026-06-15

H1's segmented arm bundled three things: horizon-reset + focused sub-prompts + per-checkpoint
retry. H2 re-runs all 3 tasks with **`SEG_RETRIES=0`** (no retry) to isolate retry's
contribution. Records: `results/h2-segretries0-glm51-n10.jsonl` (60/60 clean).

| segmented arm | pooled σ̂ | perfect | paired lift CI |
|---|---:|---:|---|
| H1 (retry=1) | 0.026 | 20/30 | +0.41 [0.25, 0.53] |
| H2 (retry=0) | 0.037 | 11/30 | +0.25 [0.13, 0.33] |

**Retry is NOT the driver.** At retry=0, segmentation STILL cuts σ̂ (monolithic 0.251 →
segmented 0.037) and STILL lifts the mean (+0.247, CI [0.13, 0.33] excludes zero).
Horizon-reset + focused-prompts carry the effect.

**Retry amplifies it** (real second-order contributor, not the cause): ~doubles the
perfect-rate (11 → 20/30) and adds ~+0.16 lift, concentrated where a failed segment can
*recover* — pov segmented 0.76 → 1.00 (1/10 → 10/10 perfect), dot_dsl 0.51 → 0.69.
**grade_school is 1.00 / 10-perfect with OR without retry** → its mean-lift (scope-completion
via the focused sub-prompt) is fully retry-independent.

**Remaining isolation (not yet run):** segmentation still bundles horizon-reset + focused
sub-prompts. The clean next ablation is **focused-prompts WITHOUT horizon-reset** — drive the
focused sub-prompts in sequence in ONE session — to separate "smaller explicit scope" from
"fresh session per checkpoint." Then H5 (cross-model, beyond glm-5.1).
