---
id: ghost-conductor-architecture
project: pillbox
type: decision
status: draft
title: ghost conductor — plan→select→execute over a typed DAG IR with exogenous reward (design, unbuilt)
related_code:
  - "scripts/ghost/**"
  - "src/commands/dispatch.rs"
---

<!-- brief:anchor ghost-conductor-architecture -->
## The conductor plans (fan-out) → selects → executes (narrow) over a typed DAG IR

ghost's conductor runs a task in two stages: **plan** (fan out k frontier
planners producing diverse decompositions, short-horizon) → **select** one plan →
**execute** (a single reliable low-variance model drives the now-rote gated chunks
in one lineage). The plan is a **typed, serializable DAG IR** (data, not code:
LLMs emit data more reliably than code; optimizers mutate data more safely) —
executed by pillbox *as data*, never as arbitrary code. The final **reward** is
conductor/task-owned, authored before the fan-out, **independent of any plan**,
anchored to exogenous criteria; per-node **gates** are plan-authored. Recovery is
**select + pivot**, never merge (merge destroys credit attribution and coherence).

**Why.** Decomposition is a proven free lift and a variance-management architecture
(shorten each chunk's horizon → drain execution variance, concentrate it in the
fannable planning stage). Keeping the plan as data is what lets the LLM planner
emit it, optimizers mutate it, and §0 attribute reward across nodes. A competing
plan must never author its own reward — that grades its own homework.

### Invariant
- The plan is data executed under a bounded capability, never arbitrary code
  pillbox runs.
- The reward is exogenous + independent of the plan; a per-node gate ≠ the reward.
- Recovery pivots to a gate-verified checkpoint and re-selects the tail; it never
  merges fragments of different plans.

> **Status: DRAFT — DESIGN-ACCEPTED BUT NOT BUILT (needs human review).** Every
> ghost decision (GD-001…GD-008 in `scripts/ghost/DECISIONS.md`) is
> "design-accepted, not yet built"; there is no conductor code to verify against.
> The only built substrate is the **degenerate path-DAG**: `dispatch --segments`
> (a single ordered chain, `src/commands/dispatch.rs`) which the DAG IR
> generalizes; the multi-node DAG executor, planner, and critic are unbuilt.
> ghost is slated to extract to its own repo (see `adr-008-ghost-extraction-trigger`),
> so this layer's decisions ultimately govern that repo, not pillbox internals.
