# Handoff — kypp findings + the Kimi-Linear-decode pipeline (for ghost)

_2026-06-21. For whoever (human or agent) continues the **ghost** meta-harness. Ghost is a consumer of
**kypp** (governed memory) and **pillbox** (substrate); this captures what we learned about kypp this
round and the Kimi-decode architecture ghost should build toward. kypp lives at `~/code/kypp`._

## TL;DR

- **kypp's mechanism is validated.** The `memory-matrix` (planted out-of-band answers, near-binary
  grader) shows all 5 governance levers — recency / authority / corroboration / scope / pitfall —
  **MEMORY WORKS**, clean at trials=1 (15/15) and trials=3. kypp's governance demonstrably changes a
  real cheap-model (glm-4.5-air) agent's behavior in the predicted direction.
- **The headline finding: dumping memory POLLUTES.** Injecting the full accepted store into the cheap
  model scores **below baseline**, robustly, across every aider held-out run. The fix is **COMPOSE** —
  task-conditioned *selection*, not a wholesale dump. **ghost's memory injection must compose, not
  `briefing`-dump.**
- **Real-task LIFT is NOT yet cleanly measured.** The cheap model's variance + baseline-at-ceiling on
  easy tasks obscure it; a single-trial "win" (bottle_song recall 1.0) did not replicate (0.05 over 3).
  Same false-positive trap as the ACE held-curve. Needs a **headroom-controlled** paired test.
- **Kimi-Linear-decode is the compose/distill stage at SCALE** — read memory wide → compress to a task
  packet. Same `compose` interface; internals scale from `semantic-recall-then-cap` → `wide-read-then-
  compress`. It drops into kypp's existing distiller and compose seams — **no ghost redesign**.

## 1. kypp — the state ghost consumes

- **Model:** governed, code-grounded **claims** (`fact|decision|procedure|pitfall|…`) with status
  (candidate→accepted), authority (human > verified > agent), corroboration gate (accept once ≥2
  independent sessions agree), and live code anchors re-resolved at recall. `observe → distill → recall
  → consolidate`. Store = embedded tursodb (concurrent writes + vector recall).
- **Agent interface is being trimmed** (see `~/code/kypp/docs/decisions/2026-06-21-agent-mcp-surface-
  compose.md`): the MCP surface goes 10 verbs → **4** (`compose` / `claim` / `expand` / `correct`),
  anchored on a new `compose` that unifies `briefing`+`recall`. ghost should target `compose` as the
  retrieval primitive (it's also the Kimi seat — §3).
- **Transcript ingestion shipped** (kypp `main`, branch `transcript-ingestion`): `kypp seed <repo>`
  bootstraps a store from a repo's `~/.claude`/`~/.codex` history; `kypp mine-tasks <repo>` mines real
  eval-task candidates from transcripts. Both feed the same `transcript → §0` front-end. (Discipline: a
  session SEEDS memory or DONATES a task, never both — else the memory contains its own benchmark answer.)
- **Distiller is swappable** behind `KYPP_DISTILL_MODEL`: heuristic floor (failure-mining) → LLM
  distiller (`claude` / `codex` / `ollama:model`; **Kimi later** — same seam). A prompt tweak now skips
  transient sandbox/env artifacts (was ~25% of distilled claims; now 0%).
- **Semantic recall needs embedded claims.** Claims distilled without an embedder have NULL embeddings →
  recall falls back to keyword (returns generic top-claims for any query). Backfill with
  `ollama_embed('nomic-embed-text')` (dim 768) over `subject\ncontent`; after that, recall genuinely
  task-conditions (fold→fold, discount→discount).

## 2. The empirical findings ghost must account for

1. **Context pollution is real and robust.** A cheap model drowns in an unselected memory dump (full
   store < baseline, every run). **Inject a SELECTED, bounded brief — never the whole store.** This is
   the single most important thing for ghost's memory loop.
2. **Measuring lift is hard; respect the variance.** glm-air swings 0↔1 on the same task across trials.
   Low-n reads lie (the ACE held-curve printed "+0.359 accrual HELPS" that was pure empty-brief
   variance). Use **paired** designs (per-task baseline vs memory, same tasks/trials), enough trials,
   and **headroom-controlled tasks** (baseline genuinely mid-range ~0.3–0.7 — not at ceiling, where
   memory can't lift, nor at floor, where nothing helps).
3. **The corroboration gate can starve the brief.** On a small/diverse train set, accept-≥2 promotes
   nothing → empty brief → no effect. Tune the gate per regime, or include high-confidence candidates
   for the ablation; don't mistake an empty brief for "memory doesn't help."
4. **Mechanism ✓, lift ?.** kypp's governance works (memory-matrix). Whether composed memory LIFTS a
   cheap model on real tasks is the open product question — not yet answered cleanly.

## 3. The Kimi-Linear-decode pipeline (the architecture)

- **Principle:** _read memory as wide as the memory is thin._ Cold/sparse store → read wide (Kimi's
  cheap long context); warm store → narrow via grounded selection. The actor never gets the raw dump.
- **Kimi-decode is a PIPELINE STAGE, not a router arm.** It runs every time as the compose/distill
  stage: wide-read → **navigational** output (sharpened task + working-set pointers + candidate claims),
  **never a substitutive lossy blob** the actor can't see past. A compression stage is a quality
  bottleneck an arm isn't — kypp's governed/grounded claims bound what it may drop.
- **Two seams, both already in kypp:**
  - **distiller** (`KYPP_DISTILL_MODEL`): corpus of transcripts/§0 logs → durable claims. Kimi's cheap
    long context distills the large dogfood backlog (`~/.claude`: ~5k sessions / 48 repos).
  - **compose** (claims → task packet): when the store outgrows a cheap recall scan, compose's internals
    become `wide-read → compress to a budget`. The `compose` MCP verb is **the Kimi seat** — same
    signature, same caller; only the selection internals change. Growth params `budget_tokens` / `role`
    live here.
- **Three orthogonal optimizers — FREEZE, don't co-optimize:** GEPA tunes prompts (compose/distill
  templates), ACE/kypp tunes claims, ghost tunes dispatch. Co-optimizing breaks attribution; alternate
  (coordinate-descent), one frozen while another tunes, on the shared `gate.py` run→score substrate.
- **Don't reach for Kimi to solve a small-store selection problem** — at tens/hundreds of claims, kypp's
  own semantic recall IS compose. Kimi is what compose becomes when recall can't scan the store.

## 4. ghost ↔ kypp integration — concrete

- **`ace.py` injects `ky.briefing()` (dump-all) today → change it to task-conditioned `compose`.** This
  is the direct application of the pollution finding: the ACE generator should inject the top-k claims
  *relevant to the current task*, not the whole accepted playbook. (We hand-rolled this as a
  `kypp-recall` arm in `run-task.sh`: `kypp recall "<task prompt>" --limit 5` with
  `KYPP_EMBED_MODEL=nomic-embed-text`.)
- **`cost-router.py` already integrates kypp** (records `route/<class>/<model>` outcomes as claims,
  recalls adequacy) — the memory-backed routing sibling of `ghost.py`. That pattern is sound; keep it.
- **`ghost.py` (router) + oracle ceiling** is the go/no-go for whether a learned router is worth it on a
  task set — run it first on any new bench before building routing.

## 5. Open experiments / next steps

1. **Headroom-controlled compose-lift test.** Pick aider (or harder) held-out tasks where glm-air's
   baseline is mid-range; paired baseline vs `compose`(top-k semantic) vs `dump-all`; enough trials +
   `paired-stats.py`. Answers the real product question (does composed memory lift a cheap model?).
2. **ACE accrual with compose injection** (`ace.py`, briefing→compose) — does the held-out curve rise as
   the playbook grows, *with task-conditioned* injection? (The dump-all version is confounded by
   pollution.)
3. **Wire Kimi as the distiller** (`KYPP_DISTILL_MODEL=…kimi…`) over the dogfood corpus, and as the
   compose compressor once the store is large.
4. **Router oracle ceiling** on the chosen bench (ghost.py) — is a router worth building here.

## 6. Artifacts + infra notes

- **ADR:** `~/code/kypp/docs/decisions/2026-06-21-agent-mcp-surface-compose.md` (the verb trim + compose).
- **kypp:** `transcript-ingestion` merged to `main`; distiller env-skip committed (`f34bd88`, local).
- **pillbox:** gate.py per-session VM-reap on branch `ace-vm-reaper` (`4a8fe30`); a `kypp-briefing` /
  `kypp-recall` arm in `run-task.sh` (worktree).
- **Eval-infra gotchas** (also in kypp's memory `pillbox-libkrun-eval-gotchas`): any bare `cargo build`
  strips the libkrun HVF codesign → use a signed copy at a stable path; `~/.local/bin/pillbox` (a release
  build) broke `bookmark list` — use a signed copy of `target/debug`; `reap-orphan-vmms.sh` is a
  dedicated-host reaper that will kill *another* pillbox's slow VMs — run your eval only when the
  cross-model campaign + its reaper are idle, and scope your own reaper to your binary's VMs.
