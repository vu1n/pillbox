# ghost — Decisions (ADR log)

The load-bearing decisions for **ghost** (the meta-harness / conductor layer that
runs on pillbox + kypp). Same contract as pillbox's `docs/decisions.md`: this is
the anti-drift record — if you think "didn't we decide X?", it's here. Changing a
decision = a new dated entry that supersedes, never a quiet reversal. Each entry
says what was **rejected**, so it doesn't get re-proposed.

> Scope note: ghost is a **tenant in this repo today** and is slated to extract to
> its own repo (see pillbox `docs/decisions.md` ADR-008). These decisions travel
> with ghost. pillbox-substrate decisions live in pillbox's log, not here.

Format: `STATUS` · what · why · what it means concretely · what's rejected.
Most are **design-accepted, not yet built** — they gate the build order, they are
not claims of shipped code.

---

## GD-001 — Conductor topology: plan (fan-out) → select → execute (narrow)
**Status: Accepted (2026-06-22, design — not yet built).**

- **Decision:** a task runs in two stages. **Plan** = fan out k frontier planners
  producing *diverse decompositions* (bounded, short-horizon). **Select** one plan.
  **Execute** = a single reliable, low-σ̂ model drives the now-rote, gated chunks,
  accumulating context in one lineage.
- **Why:** decomposition is a proven free lift (the enumerated-control +0.17); it
  is also a variance-management architecture — shortening each chunk's horizon
  drains execution σ̂ and concentrates the irreducible variance into the *fannable*
  planning stage. Fan out where it's short + high-leverage; go narrow where it's
  long + rote → strictly cheaper than fanning out whole solutions.
- **Concretely:** planning = the conductor composing the spec (the L2 layer);
  execution = the L1 swappable-worker chain consuming it. The plan **is** the spec
  (GD-002).
- **Rejected:** one monolithic agent loop for the whole task; fanning out full
  solutions (pay k× on the long part); homogeneous clones as the only fan-out.

## GD-002 — The plan is a typed DAG IR (data), not a config file
**Status: Accepted (2026-06-22, design — not yet built).**

- **Decision:** the plan is a typed, serializable **DAG IR** — nodes (per-node
  model/agent binding + prompt + a plan-authored gate), edges (deps), parallel
  branches, fan-in. Authored by a **code builder** (humans) or **emitted as
  structured data** (the LLM planner); **executed by pillbox as data** (bounded
  capability), never as arbitrary code. The conductor (select/drive/gate/pivot) is
  code; the *plan* is data.
- **Why:** flat config can't express a DAG without becoming a bad programming
  language (the YAML/HCL trap). But the plan must stay *data* because the whole
  strategy rests on it: the LLM planner **emits** it, optimizers **mutate** it,
  kypp **stores** winning plans, §0 **attributes** reward across nodes. LLMs emit
  structured data more reliably than correct code; optimizers mutate data more
  safely than they do code; "the plan is the IP / a System of Configuration" needs
  a serializable value. The pattern (code DSL → graph IR) is Airflow/Dagster/Bazel.
- **Concretely:** the IR schema is the contract (the `agent.proto` analog for
  plans). Linear `dispatch --segments` is the *degenerate path-DAG* — ship that
  subset first; add parallel/branch only when a task needs it. The **DAG executor**
  is pillbox (mechanism, generalizing `dispatch --segments`); the **builder +
  planner** are ghost.
- **Rejected:** `harness.toml` as the canonical plan format; "code-based" meaning
  *plan = arbitrary code pillbox runs* (capability + boundary regression).

## GD-003 — Reward is exogenous + independent; gates are plan-authored
**Status: Accepted (2026-06-22, design — not yet built).**

- **Decision:** the final **reward** is conductor/task-owned, authored **before**
  the planning fan-out, **independent of any plan**, and anchored to **exogenous**
  criteria (real tests / human acceptance / characterization tests of observed
  behavior). Per-node **gates** are plan-authored (the plan's own falsifiable
  checkpoints). gate ≠ reward.
- **Why:** if a competing plan authors its own reward, it grades its own homework —
  a lenient reward wins by *looking* done. The competitor must never define the
  finish line it's judged against. Verifier > judge on verifiable work: execution-
  grounded + forge-resistant + you **audit discriminating power** (mutation /
  negative + positive examples) rather than trust the author. The regress ("who
  verifies the conductor's reward?") bottoms out at the exogenous anchor.
- **Concretely:** maps onto `dispatch --segments`' existing shape (per-segment
  gates + a separate, required, run-level reward) — this just pins *who authors the
  reward*: the conductor/task, never the plan. Plans own the **map**, the conductor
  owns the **destination**.
- **Rejected:** "each plan emits its own verifier" (the reward half is the bug);
  a frontier model as **judge** for the reward (judge is the *fallback* for the
  genuinely unverifiable residue only, trusted less, and must NOT train optimizers
  at verified-weight).

## GD-004 — Recovery is select + pivot, never merge
**Status: Accepted (2026-06-22, design — not yet built).**

- **Decision:** carry one coherent plan; on plan-level failure, **pivot** = rewind
  to the last gate-verified checkpoint and **re-select the tail** (swap the unproven
  remainder for a sibling plan). Never merge fragments of different plans. Recovery
  ladder: retry-within-chunk → pivot-the-tail → re-plan fresh, under a bounded pivot
  budget.
- **Why:** merge produces a disjoint context lineage (a chunk inherits state that
  doesn't match the plan it now follows) and destroys credit-attribution (was it
  A's chunk-1 or B's chunk-2?). Selection keeps one coherent lineage = clean
  execution **and** clean plan-level credit (the signal a learned conductor trains
  on).
- **Concretely:** a passing gate is a checkpoint (snapshot/bookmark); pivot rewinds
  to it and re-selects from the fan-out's runner-ups (the "pivot reserve"). The
  circuit-breaker (GD-006 / pillbox §0) is the pivot trigger. On pivot-budget
  exhaustion: escalate to frontier-execution or surface "task mis-specified."
- **Rejected:** merging best-fragments-each-round (Frankenstein decomposition);
  unbounded pivoting (thrash).

## GD-005 — Selection uses a grounded critic, not a judge
**Status: Accepted (2026-06-22, design — not yet built). Calibration-gated.**

- **Decision:** in-run selection / early-stop (where the true reward is held out)
  uses a **critic calibrated against the exogenous reward** — and trained on
  **execution-grounded gate outcomes**, not judge-annotated labels. The critic is a
  *calibrated estimator* of the reward, never the reward itself.
- **Why:** OpenHands' critic (best-of-8 +15.9 pts) shows a critic earns its place
  for in-run selection — but theirs is trained on judge-annotated rubrics (a learned
  judge, Goodhart-prone) because a flat agent has no grounded intermediate signal.
  **ghost's gated DAG produces exactly that grounded signal**, so ghost can train
  the same architecture on gate outcomes and get the lift without the
  grade-your-own-homework loop. Lower scorer noise (σ) also *buys beam width*
  (GD-006).
- **Concretely:** select on probe-against-the-independent-reward + cross-fan-out
  **convergence** + decomposition coherence + the critic — never on whose gates are
  easiest to pass. Train on §0 traces + real outcomes; report calibration (AUC,
  ECE); the exogenous reward, not the critic, picks among survivors.
- **Rejected:** a judge (prompted LLM scoring) as the selector of record; training
  the critic on judge labels; letting the critic *be* the reward.

## GD-006 — Grounded beam search is gated and LAST in the build order
**Status: Accepted (2026-06-22, design — not yet built). Explicitly deferred.**

- **Decision:** at high-uncertainty decision points, the conductor may **fork from
  a gate-verified snapshot** and explore top-k coherent continuations (beam search
  over the snapshot-lineage DAG; select+pivot is the width-1 case). This is the
  **last** execution feature, **default-off**, and **gated on critic calibration**.
- **Why:** the prior art (LATS, ToT, SWE-Search, AlphaCode, Fork/Explore/Commit,
  forkd, ConTree) shows this is novel-in-integration only, *and* that it has a
  narrow win condition: best-of-N usually beats tree search; "More Test-Time Compute
  Can Hurt" — a wider beam over a noisy scorer selects *worse* paths (threshold
  n̂ ≈ 1 + exp(Δ²/2σ²)); value error compounds with depth. ghost's grounding
  defeats both ceilings (gate-grounded critic → lower σ → wider affordable beam;
  passed gates are zero-noise re-anchors → bounded depth-compounding) — but only
  *after* the critic is calibrated enough to afford width > 1.
- **Concretely:** **build order = linear gated execution → best-of-N (fork-k at
  start) → grounded beam (last).** Set width by the n̂ rule (start k=1, earn width);
  residual/marginal value not absolute; Successive-Halving/ASHA budget allocation;
  semantic dedup by outcome; exogenous reward picks survivors. Don't reinvent the
  fork primitive — mine Crab / Shepherd / Fork-Explore-Commit / ConTree / forkd.
- **Rejected:** building the beam first / assuming tree-search beats best-of-N;
  fixed beam width independent of critic quality; the beam as a moat (the search is
  commoditizing — the substrate is the edge).

## GD-007 — Optimizer nodes: optional, σ̂-gated, default-off
**Status: Accepted (2026-06-22, design — not yet built). Bakeoff-gated.**

- **Decision:** the DAG may contain **optimizer nodes** parameterized by
  `(target, trainset, metric, accept-gate, strategy)`. GEPA / DSPy / MIPRO / ACE /
  RLM are *strategies* on one contract ("optimize anything"). Optional, default-off,
  gated on a 3-arm bakeoff clearing the σ̂ wall.
- **Why:** the optimization-layer verdict holds — the literature has ~zero
  coding/sparse-reward results, so trusting the optimizer is an unproven transfer
  bet. Having the node *type* is cheap; trusting it is earned.
- **Concretely:** **never optimize the reward** (it's the invariant they serve;
  optimizing it = Goodhart catastrophe). **Held-out walled off** (the node gets
  train, never the eval-gate; accept only on held-out lift). **Mostly offline** (a
  slow meta-DAG across many tasks emits improved components future runs adopt;
  inline = n=1 overfit). Targets = node prompts, the **planner itself** (= the
  learned conductor, the north-star), worker weights (RLM = RLVR using ghost's
  verifier as the reward), kypp claims, the selection policy.
- **Rejected:** an always-on learned conductor before the bakeoff; optimizing
  against a judge-derived or un-audited reward.

## GD-008 — Eval/bench: borrow OpenHands' harness, keep ghost's variance rigor
**Status: Accepted (2026-06-22, design — not yet built).**

- **Decision:** build the eval/bench layer on OpenHands' shape — an abstract
  `Evaluation` base + per-bench subclass (`prepare_instances` /
  `prepare_workspace` / `evaluate_instance`), **SWE-bench Verified as the anchor**
  (delegate scoring to the official `swebench.harness`; its repo tests = the
  exogenous, held-out reward of GD-003), expanding toward the 5-category map
  (Issue-Resolution / Greenfield / Frontend / Testing / Info-Gathering). **Keep
  ghost's variance discipline** as the differentiator: N≥10/instance, paired-stats
  / σ̂ / sensitivity-gate (CI excludes 0), and a flaky-instance filter
  (SWE-bench-Live style: keep only consistent-across-runs).
- **Why:** OpenHands is the mature reference for bench infra, but reports single-run
  **point estimates** — and the fresh variance literature (2602.07150, 2603.25764)
  now backs ghost's σ̂ thesis. Borrow their infra; do **not** copy their reporting.
  SWE-bench's pre-existing held-out tests are the cleanest instantiation of an
  exogenous anchored reward.
- **Concretely:** grow `pillbox eval` (variants×trials today) into the suite runner;
  align the JSONL result/cost schema; add seed isolation + multi-run (the
  reproducibility OpenHands lacks). Steal their **condenser** (context-window
  summarization) into §0/kypp for long-horizon.
- **Rejected:** point-estimate reporting; reimplementing the SWE-bench scorer
  (delegate to the official harness).

---

## Open (not yet decided — do not record as decisions)

- **Planner authoring surface at maturity:** code-builder DSL vs LLM structured-emit
  is *both* (both target the GD-002 IR), but the *primary* path and the DSL's exact
  shape are unsettled.
- **Critic substrate:** prompted vs trained (4B-for-early-stop vs larger); TD
  discount γ unvalidated for the domain.
- **Adaptive beam-width policy:** the fork-threshold function (when score-spread
  warrants width > 1) is unspecified.
