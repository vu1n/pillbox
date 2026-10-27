---
id: adr-008-ghost-extraction-trigger
project: pillbox
type: decision
status: active
title: ghost extracts to its own repo; trigger = conductor-contract stability
supersedes: docs/decisions.md#ADR-008
related_code:
  - "scripts/ghost/**"
  - "scripts/eval/**"
  - "src/commands/dispatch.rs"
---

<!-- brief:anchor ghost-extraction-trigger -->
## ghost stays an in-repo tenant until the conductor contract stabilizes

ghost (the meta-harness / conductor layer — `scripts/ghost/`, the task/verifier
corpus, orchestration policy) becomes its own repo — but **not now**. The trigger
is **contract stability, not the calendar**: extract once the pillbox primitives
the conductor depends on have settled (§0-subscribe-consumed-by-a-conductor, the
intervention hooks, and the DAG executor + per-worker spec generalizing
`dispatch --segments`). Until then ghost is an **in-repo tenant** treated as a
*contract consumer* (the `pillbox`/`kypp` CLIs + the §0 event schema), never
coupled to pillbox internals.

**Why.** Extracting now makes every mid-churn primitive (§0-subscribe wiring,
kill hooks, the DAG executor) a cross-repo release dance, and there is nothing
built to extract yet. A separate repo *later* can only touch what pillbox exposes
as a real interface, which structurally prevents baking the conductor LLM into
the substrate.

### Invariant
- ghost lives under `scripts/ghost/` as a clean tenant (own README/DECISIONS/deps)
  and does **not** reach into Rust internals — so extraction is a
  `git filter-repo` move, not a rewrite.
- The **DAG executor stays in pillbox** (mechanism — the generalization of
  `dispatch --segments`); the IR schema is a shared contract; the builder +
  planner + pivot policy + frozen corpus go to ghost.
- ghost's own design decisions live in `scripts/ghost/DECISIONS.md`, not in
  pillbox's decision log.
