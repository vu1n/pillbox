# σ̂-segmentation experiment (GHOST-006/007)

The **keystone** of the variance frame (`docs/optimization-gate.md`,
[[pillbox-optimization-layer-verdict]]). One question:

> Does cutting a long-horizon coding task into **rubric-gated segments** reduce
> the trial-to-trial variance **σ̂** — the bistability that long agentic horizons
> exhibit, and the cost best-of-k otherwise pays to overcome?

If yes, segmentation is a real lever and ghost's design holds. If no, the frame
needs rework before any further build. GHOST-006 builds this harness; GHOST-007
runs it and writes the verdict.

## The two arms

Both arms run the **same task** and are scored on the **same authoritative full
rubric**, so their scores are directly comparable. The only thing that differs
is the horizon:

| Arm | Horizon | Sessions |
|---|---|---|
| **monolithic** | the whole task in one shot | 1 session |
| **segmented** | one checkpoint per segment | a *fresh* session per segment |

The segmented arm runs each segment in a **fresh session over the prior
segment's verified workspace** — the horizon is *reset* at every checkpoint.
That reset is the point: driving all segments in one session would let context
accumulate and would **not** reset the horizon, so it wouldn't test the
hypothesis. Each segment is gated by an authoritative sub-rubric (retried up to
`SEG_RETRIES`); the gate only steers whether the arm advances — the comparable
score is the full rubric graded at the end, so a coarse gate can't bias the
metric.

## Relationship to `pillbox dispatch`

This harness is dispatch's **segmentation sibling**. Both compose the same
`run → drive → score → pull` primitives (GHOST-004 de-risked them live on
libkrun), but:

- `pillbox dispatch` **forks** one horizon `k` ways and selects the best
  (best-of-k — turns σ̂ into gain at selection time).
- this harness **chains** short horizons (segmentation — cuts σ̂ at the source).

They compose: `SEG_K>1` would run best-of-k *per segment* on top of segmentation
(a follow-up lever). The default **`SEG_K=1` isolates segmentation** as the
single variable under test, which the keystone measurement requires — a positive
result under `k>1` could be best-of-k, not segmentation.

## A segmented task = a task + a segment decomposition

The harness reuses the existing `run-task.sh` task format (`prompt.txt` +
`workspace/` + hidden `grader/`) and adds an ordered **segment spec** under
`segments/<task>/`:

```
segments/<task>/
  01-<name>/
    prompt.txt    # this segment's focused instruction (no test leaked)
    rubric.txt    # the segment GATE — an authoritative SUBSET of the task's
                  #   hidden tests (NAME :: COMMAND; the harness injects the
                  #   hidden grader at grade time, so the agent never sees it)
  02-<name>/
    ...
```

The bundled example is **`ap_pov`** (Aider-polyglot "Pov" — reorient a tree):

- **`01-reroot`** — `Tree.from_pov` (8 of the 15 hidden tests).
- **`02-pathfind`** — `Tree.path_to`, which *builds on* `from_pov` (the other 7).

`path_to` genuinely depends on `from_pov`, so segment 2 forks from segment 1's
verified workspace — a real sequential dependency, not an artificial split. And
`ap_pov`'s long horizon makes its monolithic arm bistable (high σ̂ in prior
same-condition runs) — the **headroom** a cut needs to land in. The segment
gates reference the task's *own* `pov_test.py` methods (authoritative subsets),
so a weak hand-written gate can't be the confound.

## Run it

```sh
# the gate: print the trial matrix, resolve every path, launch nothing
scripts/eval/segmentation/run.sh --dry-run

# live (GHOST-007). Needs: a codesigned libkrun binary (scripts/lk-build.sh),
# opencode authed, the runner image, and the task materialized:
python3 scripts/eval/import-aider-polyglot.py --limit 20   # materializes tasks/ap_pov
PILLBOX_RUNNER_IMAGE=pillbox-runner:dev MODEL=zai-coding-plan/glm-4.5-air \
  scripts/eval/segmentation/run.sh --trials 10
```

Output: JSONL trial records `{task, cond, trial, score, cost}`, piped to
`../paired-stats.py` (the paired lift CI + pooled σ̂) **and** a per-arm σ̂ summary
— `sigma_hat[monolithic]` vs `sigma_hat[segmented]` and `delta_sigma`, the
keystone headline.

### Knobs

| Env | Default | Why |
|---|---|---|
| `TRIALS` | 10 | n per arm (≥10 for a usable σ̂). |
| `TEMPERATURE` | 0 | greedy — isolate segmentation from sampling noise. |
| `MAX_WAIT` | 600 | generous: truncating the monolithic arm's long horizon inflates its failure (a known σ̂ confound), faking a segmentation win. |
| `SEG_RETRIES` | 1 | per-segment gate retries before advancing anyway. |
| `SEG_K` | 1 | (reserved) best-of-k per segment — the dispatch lever, a follow-up. |
| `MODEL` | opencode default | set a capable model so the monolithic baseline lands in a measurable band. |

## Reading the verdict (the confounders to control)

Per arXiv 2603.29231, calibrate before calling a result:

1. **Effect size.** SOTA parallel+sequential methods buy single-digit to
   low-double-digit point gains. A small-but-real σ̂ cut is **success**, not a
   null.
2. **Verifier quality is a co-variable.** A weak gate adds latency without
   cutting variance. We pin the gates to the task's own hidden tests; if you add
   tasks, keep gates authoritative.
3. **Headroom.** A saturated family (toolz) has no capability-variance to
   express, so segmentation can't show a delta. Use long/bistable tasks.

GHOST-007 records the verdict (`segmentation cuts σ̂: yes/no + effect size`) into
`docs/optimization-gate.md` and the raw records under `results/`.

## Adding a task to the family

1. Materialize or author the task dir (`prompt.txt` + `workspace/` + hidden
   `grader/` with a `rubric.txt` of per-test criteria).
2. Decompose it into 2–3 genuinely sequential segments under
   `segments/<task>/NN-<name>/{prompt.txt,rubric.txt}`, each gate an authoritative
   subset of the task's tests.
3. `run.sh --dry-run <task-dir>` to confirm the matrix resolves, then run live.
