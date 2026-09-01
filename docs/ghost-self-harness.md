# ghost self-harness arm — spec (not yet built)

The orchestrator-optimization arm of ghost, mapped onto pillbox's primitives.
Recipe lifted from **Self-Harness** (arXiv 2606.09498, June 2026): an agent that
weakness-mines its own failed traces, proposes *minimal harness-scaffold diffs*, and
promotes them only through a held-in/held-out regression gate.

> **Status: SPEC, gated.** Do NOT build yet. This is what to wire *after* the
> de-risking prerequisite passes (see §6). The same σ̂≈0.35 variance wall that parked
> the GEPA bet (`docs/optimization-eval-family.md` §5, the optimization-layer-verdict
> memory) gates this too. Writing it down now so it's ready, and so the one place we
> *improve* on the paper (the variance gate, §4) is captured.

---

## 0. The one distinction that matters

The parked gate (`gate.py`) optimized the **worker prompt** — a `PROFILE.md` prepended
to the task — and found it to be noise on our task family. Self-Harness optimizes a
**different layer**: the worker's *harness scaffold* (instructions + runtime control
policy + subagent defs), as code-shaped diffs, never the prompt-prepend. The paper's
own promoted edits are control-flow, not prose:

- GLM-5: "preserve environment settings across shell commands"
- Qwen3.5: "avoid repeated failed commands, break cycles of endless exploration"
- MiniMax: "stop unproductive tool-use loops, handle structured tool outputs carefully"

None of those is a profile bullet. They're *scaffold/runtime-policy* edits — a surface
the gate never touched. That's why a null worker-prompt result does **not** transfer
here, and why this arm is worth specifying despite the parked verdict.

This also matches ghost's clarified north star (the metaharness-north-star memo):
ghost optimizes the **orchestrator/harness**, not worker prompts.

---

## 1. The editable harness surface (`h₀ → h₁ → …`)

A versioned **harness manifest** — the learned artifact, mutated by diff each round.
Three surfaces, in increasing order of plumbing cost:

| Surface | Concretely | Buildable today? |
|---|---|---|
| **Instructions** | an `AGENTS.md` written into the workspace clone before the run (opencode reads it): bootstrap / execution / verification / failure-recovery guidance | **Yes** — just a file in the clone (same seam as `graded_run` copying the grader in) |
| **Runtime policy** | step/tool budgets, loop-break thresholds, verify-before-done gate | **Partially** — `--max-wait` exists; step caps / loop-break / tool budgets need opencode-config plumbing into the `pillbox run` launch (today unexposed) |
| **Subagent / skill defs** | opencode subagents/commands the worker can call | Later — config surface exists in opencode, not wired through pillbox |

v1 arm = **instructions surface only** (faithful to the paper's "system prompt +
bootstrap/exec/verify/failure-recovery instructions", and shippable with zero substrate
changes). The runtime-policy surface — the genuinely-new control-flow edits that make
Self-Harness more than prompt-tuning — is the v1.5 follow-on once opencode config is
threaded through `run` (track it as the substrate dependency).

Manifest shape (a directory pillbox can mount + a small JSON the applier reads):

```
harness/
  AGENTS.md            # the instructions surface (the v1 mutation target)
  policy.json          # {max_steps, loop_break_after, verify_before_done, tool_budget}  (v1.5)
  lineage.jsonl        # h₀…hₙ: {round, parent, diff, accepted, held_in_Δ, held_out_Δ, σ̂, ci}
```

---

## 2. The loop, mapped to our primitives

```
                 ┌─────────────────────────────────────────────────────────┐
   held-in  ───▶ │  Generator   run worker under hₜ, grade            ─┐    │
   tasks         │              (gate.py: Pillbox.session+drive+score) │    │
                 │                                                      ▼    │
                 │  Weakness    read FAILED §0 traces + grader feedback,     │
                 │  Mining      cluster by verifier-grounded signature φ     │
                 │              (session log --type tool_call,scored;        │
                 │               reflector model = frontier)                 │
                 │                                                      │    │
                 │  Proposal    reflector emits K minimal+diverse DIFFS  ▼   │
                 │              to the manifest (NOT a prompt prepend)       │
                 │                                                      │    │
   held-out ───▶ │  Validation  apply each hⱼ; score on held-in AND     ▼   │
   tasks         │  + σ̂ gate    held-out; accept iff §4 passes               │
                 └─────────────────────────────────────────────────────────┘
```

| Self-Harness stage | ghost does | reuse |
|---|---|---|
| **Generator** | run the worker under `hₜ`, grade it | `gate.py::Pillbox.session/drive/score`, `graded_run` (drop the `profile` prepend; mount `harness/` instead) |
| **Weakness Mining** | read failed runs' §0 trajectory + per-criterion grader feedback; cluster by `φ = (failed rubric criteria, agent mechanism from tool_call trace, terminal cause)` | `pillbox session log --type tool_call,message_end,scored` / `session ingest`; richer than gate's feedback-only `failures` (it sees the *trajectory*, which is where "loops on failed commands" is visible) |
| **Proposal** | frontier reflector writes K minimal, materially-distinct **manifest diffs** + a rationale (targeted failure, edited surface, expected effect, regression risk) | generalize `gate.py::distill` — same reflector-reads-failures mechanism, output target changes from `PROFILE.md` to `AGENTS.md`/`policy.json` diffs |
| **Validation** | apply each `hⱼ`, score held-in + held-out, accept per §4 | `gate.py::eval_arm` ×2 splits + `paired-stats.py` |

The frontier reflector / cheap worker split already exists (`--reflector-model` vs
`--worker-model`): the cheap model does the volume, the frontier model is spent only at
mine+propose time. Same teacher→student economics as the gate.

---

## 3. Weakness Mining — verifier-grounded, not vibes

The paper's φ groups two failures together only when they agree on **what the verifier
rejected** + **how the agent's behavior caused it**. We have both halves cheaply:

- *What the verifier rejected* — `session score --rubric` already returns per-criterion
  `{name, passed, feedback}`. The failed criteria names ARE the terminal-cause cluster key.
- *How the agent caused it* — the §0 `tool_call` stream (`session log --type tool_call`)
  is the trajectory. Mechanism signatures are pattern-matchable host-side before the
  reflector even sees them: repeated identical failed `bash` calls → "loop"; zero test
  invocations before idle → "no-verify"; truncated/!idle → "budget".

So mining is *deterministic clustering first* (host-side over §0 + criteria), reflector
*second* (writes the diff for the top cluster). Keeps the frontier call small and the
cluster key grounded — exactly the paper's discipline.

---

## 4. The accept gate — where we IMPROVE on the paper

Self-Harness's rule: accept `hⱼ` iff `Δheld-in ≥ 0 AND Δheld-out ≥ 0 AND max(Δ) > 0`,
held-out never shown to the proposer. Conservative — but the paper reports **no seed
variance, no error bars, no ablations**, on ~32-task splits where +21pts ≈ 7 tasks. On
our measured σ̂≈0.35 regime that gate would promote noise (a 7-task swing is *inside* the
same-condition flake we measured: 0.067↔0.733).

**Our gate = the paper's rule wrapped in the variance discipline it skipped.** Reuse
`paired-stats.py`: each split scored over `--trials` at temp-0; accept `hⱼ` iff

```
held-in lift-CI > 0  AND  held-out lift-CI ≥ 0 (excludes a regression)  AND  σ̂ ≤ 0.10
```

i.e. the lift must clear the bootstrap CI, not just the point estimate. If σ̂ > 0.10 the
round is **inconclusive, not accepted** (the §5-stop-branch from the eval family doc).
This is the single thing that makes a self-harness loop trustworthy on a noisy worker —
and the reason §6's prerequisite is non-negotiable.

---

## 5. The memory→scaffold promotion path (kypp integration — our edge)

Self-Harness has **no persistent memory**; the harness lineage is its only artifact. We
have kypp. The two are complementary, and the seam between them is a new idea the paper
exposes:

- **kypp (per-task, dynamic)** — facts retrieved into *this* task's context via
  `kypp briefing` (repo conventions, a specific API's gotcha). Stays in the brief.
- **harness manifest (baked-in, static)** — model-specific, task-*general* policy that
  should apply to *every* run ("this worker loops on failed shell commands → loop-break
  after 2"). Stays in `AGENTS.md`/`policy.json`.

**Promotion rule:** a kypp claim that corroborates across ≥N sessions *and* is
task-general (no task-specific anchors) **graduates** from "context you brief" to
"policy baked into the harness diff." Mechanically: `ace.py::Kypp.curate` already
promotes corroborated claims; add a `--promote-to-harness` that, for the task-general
corroborated set, emits a manifest diff candidate into the *same* §4 gate. So the
manifest and the playbook share one acceptance protocol; the manifest is just the
subset of memory that earned permanent residency.

This also gives a demotion path: a baked-in policy that later *fails* the §4 gate on a
fresh split gets reverted out of the manifest (the lineage records it) — the scaffold
analog of `kypp reject`.

---

## 6. Prerequisites & gating (READ THIS BEFORE BUILDING)

This arm inherits the exact blocker that parked GEPA/ACE:

1. **A headroom regime** — a frozen task family where the worker partially succeeds and
   the rubric gives a gradient (not the binary all-or-nothing of toolz, not the σ̂=0
   triviality of the synthetic family). Without headroom there's no lift to mine.
2. **σ̂ ≤ 0.10 on that family** — the §4 gate is meaningless above it. The toolz family
   measured σ̂=0.346 on hosted glm; the open path is a genuinely deterministic worker
   (a local greedy model), not more trials.

Until both hold, this stays a spec. Self-Harness's published lift *raises the odds the
bet pays* (first positive coding/small-model result) but does **not** clear our
measurement bar — they skipped it. So the move is unchanged: run the de-risking
experiment (deterministic worker × headroom family), and *only if* σ̂ drops below 0.10
with a detectable planted lift, wire this arm.

---

## 7. Build order (when unblocked)

Minimal first slice, instructions-surface only, reusing the rig wholesale:

1. **`scripts/ghost/self_harness.py`** — skeleton mirroring `ace.py`: import
   `gate.Pillbox/_task_dir/bookmarks/eval_arm`; load/mount a `harness/` manifest instead
   of prepending a profile.
2. **Mining** — host-side φ-clustering over `session log --type tool_call,scored` +
   per-criterion feedback (the deterministic half), feeding the reflector.
3. **Proposal** — generalize `gate.distill`: reflector writes an `AGENTS.md` diff (K
   candidates), not a `PROFILE.md`.
4. **Gate** — `eval_arm` on held-in + held-out ×trials → `paired-stats.py` → §4 accept.
   Append to `harness/lineage.jsonl`.
5. **`--self-test`** — the accept-gate math (planted lift accepted, null/high-σ̂
   rejected), no agent — same discipline as `ace.py`/`paired-stats.py` self-tests.
6. *(v1.5, substrate-gated)* thread opencode runtime config through `pillbox run` so the
   runtime-policy surface (loop-break, step budget, verify-before-done) becomes editable —
   the control-flow edits that are the paper's real contribution.
7. *(v2)* mutate the **ghost orchestrator** itself (router/decompose policy in
   `ghost.py`) under the same loop — overlaps with the DSPy route-only router; do it
   after the worker-harness arm proves the loop end-to-end.
