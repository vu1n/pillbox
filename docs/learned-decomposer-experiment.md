# Experiment sketch: the learned decomposer

**Can the harness *learn* the task decomposition it currently hand-authors — from
its own failed trajectories, without seeing the answer — and does that learning
*compound* (transfer across tasks, persist across sessions/actors via shared
memory)?**

This is the experiment the enumerated-monolithic control sets up
(`docs/optimization-gate.md` §2026-06-19) and the operationalization of the goal:
a harness that learns and improves, with multiplayer + shared memory (kypp) as the
compounding substrate.

## The setup it inherits

The enumerated control proved a **hand-authored** decomposition (a vague README →
ordered sub-goals with signatures, contracts, error specs) lifts a headroom task
**+0.17** and cuts **σ̂ −0.196** — a static, instruction-layer win, no loop. A human
wrote that structure *knowing the solution shape*. The question now: how much of
that lift can a **learned** decomposer recover *without* the answer, and does it
stick?

So the comparison is anchored at both ends:
- **monolithic** (vague README) = floor.
- **enumerated** (hand-authored decomposition) = **oracle ceiling** — "recovered X%
  of the authored lift" is the interpretable metric.

## Why this is a *different* experiment than the gate that came back negative

The 2026-06-03 optimization gate killed GEPA/ACE → "no headroom." This is GEPA-shaped
but differs on the three axes that explain that negative:

1. **The module optimized is the *decomposer*** (vague task → structured sub-goal'd
   prompt), not a worker-instruction tweak — the layer
   [[pillbox-metaharness-north-star]] says holds the leverage ("DSPy optimizes the
   orchestrator/decomposer, not worker prompts").
2. **Headroom / vague-prompt regime** (monolithic 0.13 pass), not the gate's
   no-headroom / already-good-prompt regime (baseline 0.69) where an optimizer
   provably can't help.
3. **Rich traces.** The gate's #1 caveat was a thin trajectory (tool names + statuses
   only). This feeds the full message + tool-call trajectory via `session ingest` —
   the structure a proposer needs to diagnose a bad decomposition.

## The decomposer module

- **In:** vague task prompt (README) + recalled decomposition patterns from kypp
  (warm condition only).
- **Out:** a structured enumerated prompt (the enumerated-arm *shape*), produced by
  an LLM "decomposer" call.
- **Execute** the task with that generated prompt; grade with the full rubric.
  Start **one-shot, no gates**, to isolate the decomposer from the separable,
  already-proven +0.18 checkpoint-gating lever (`dispatch --segments`).
- **Decomposer model — a dimension:** same-model (glm-5.1 decomposes + executes) vs
  **frontier-decomposer → small-worker** (teacher→student,
  [[pillbox-small-local-model-direction]]; composes with H5 cross-model).

## The learning loop (GEPA, execution-grounded)

1. decompose → execute → grade (rubric score + per-criterion feedback).
2. on low score: feed the decomposer the **rich trajectory** (`session ingest` →
   messages + tool calls) + the rubric feedback, prompt it to **reflect** — "your
   decomposition produced these failures; revise it" → a mutated decomposition. The
   gradient is the **verifiable rubric + the trace**, NOT the hidden tests/solution
   (the integrity line).
3. keep-best / Pareto across attempts (GEPA selection).
4. **distill** the winning decomposition into a kypp memory: a model-agnostic,
   structured **pattern** (sub-goal skeleton + spec template) — signal-not-content,
   NOT solution code (Parallel-Distill-Refine / kypp distill discipline,
   [[kypp-closed-loop-hygiene]]).

## The compounding dimension — the actual point (multiplayer + shared memory)

What separates "an optimizer" from "a harness that learns":

- **Transfer (warm vs cold)** — learn decompositions on a TRAIN split; on a **LOCKED
  TEST split**, compare cold (empty kypp) vs warm (kypp pre-loaded with
  train-learned patterns). **warm > cold on held-out = the learning transfers and
  compounds.** This is the experiment the gate never ran.
- **Pooling (multiplayer)** — decompositions produced/validated by N parallel worker
  sessions/actors, pooled into kypp under **corroboration-accept** (≥2 sessions
  confirm a pattern lifts before it's trusted — the kypp arbiter,
  [[pillbox-ghost-memory-engine]]). pooled vs single-session. Gated behind transfer
  working.

## Measurement — three distinct claims

1. **Lifts at all?** learned vs monolithic (floor) vs enumerated (ceiling) on
   held-out → % of authored lift recovered. Per-arm σ̂ + per-task read (NOT the
   degenerate n=3 bootstrap CI — the review's lesson).
2. **Compounds / transfers?** warm > cold on the LOCKED test split.
3. **Multiplayer pools?** pooled > single (advanced; gated behind #2).

## Integrity controls (don't re-overclaim)

- **Locked test split** (the gate's missing piece): learn on train, evaluate on
  never-seen test; zero selection pressure on test.
- Decomposer **never sees hidden tests / solutions** — learns only from its own
  trajectory + rubric feedback.
- **Oracle ceiling** (enumerated) calibrates every number.
- **Cost honesty:** GEPA loops are expensive. Measure **lift-per-dollar vs the cheap
  baseline** — hand-author the decomposition once + reuse it. The learned loop must
  beat author-once-and-reuse to justify itself; otherwise just ship the authored
  skill.
- ≥8–10 headroom tasks before any CI; report per-task.

## Kill / build criteria

- **learned ≈ monolithic** (recovers ~0% of the authored lift) → the decomposer
  can't discover structure from traces → the gate's negative stands; ship
  hand-authored decompositions only (the skill).
- **learned recovers a meaningful fraction AND warm > cold on held-out** → the
  harness genuinely learns and compounds → build the learned decomposer into ghost
  (the meta-harness thesis, confirmed on execution-grounded reward).
- **learned lifts but warm ≈ cold** → a per-task optimizer, not a compounding
  memory → useful, but doesn't justify the shared-memory story; ship as a per-task
  helper and reconsider the memory loop.

## Primitives (all exist today)

dispatch / segmentation harness (drive + grade) · `session score --rubric` (the
reward + textual-gradient feedback) · `session ingest` / `session log` (rich traces
— the gate's missing input) · kypp store/distill/recall/corroborate (shared memory)
· frozen aider-polyglot tasks (`import-aider-polyglot.py` / `freeze-task.sh`) · the
enumerated arm (oracle ceiling) · `paired-stats.py`.

## Sequencing — where this sits

**NOT next.** Ship first (proven, cheap): `dispatch --segments` (the +0.18 gating
lever, `docs/dispatch-segments-design.md`) + the hand-authored decomposition skill
(the free +0.17, GHOST-005). **Then** this is the research arm that asks whether the
+0.17 can be *learned and compounded* instead of authored — distinct from H5
(cross-model robustness of what we already have). Prerequisites: rich-trace capture
verified end-to-end (`session ingest` on the worker family), a locked test split
built, ≥8 headroom tasks decomposed.

## Provenance

- The win this learns to reproduce: `docs/optimization-gate.md` §2026-06-19
  (enumerated control).
- The conceptual reconciliation (why this reopens GEPA): memory
  [[ghost-learned-decomposer]].
- The layer it targets: [[pillbox-metaharness-north-star]],
  [[pillbox-optimization-layer-verdict]].
