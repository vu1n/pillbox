# Optimization-layer gate — result + decision

**Date:** 2026-06-03 · **Decision: do NOT build the DSPy/GEPA meta-harness.**

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
