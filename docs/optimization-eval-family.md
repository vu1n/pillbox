# Eval Task Family for the Optimization-Layer Bakeoff — Design

Status: design (2026-06-06). Companion to [optimization-gate.md](./optimization-gate.md)
and [swarm-memory.md](./swarm-memory.md). No new substrate — builds on the shipped
eval rig (`scripts/eval/`), the `Scored` §0 event, and rustic freeze.

## 0. The problem (one sentence)

The gate that decides whether to build the meta-harness **can't resolve a lift
because its only task family is simultaneously high-variance and saturated** — the
two failure modes compound. Every prior run came back "all arms within noise":

- gate (glm-5.1): baseline 0.690, GEPA 0.702, ACE 0.706 — spread 0.016, single-task
  swings ±0.5, `connect` flipped 0.6↔1.0 same-condition.
- small-worker (glm-4.5-air): SE≈±0.11 at trials=2; tasks swing 0↔0.95.
- two-identical-condition noise floor measured at **~0.24** at trials=2.
- kypp lift: same-condition runs scored **0.067 / 0.333 / 0.733** — "−0.266 lift was
  variance."

So this is **not** "pick a harder benchmark." It's: design a family whose
per-condition variance is small relative to a plausible lift, with **headroom** (a
strong base doesn't already score ~0.7), **freezable/contamination-resistant**,
**representative** (in-repo coding edits), scored through `session score`. And
before any of that — build the **gate-before-the-gate**: prove the rig can detect a
*known, injected* difference at all.

## 1. Variance diagnosis

Four stacked sources, with the knob that cuts each:

1. **Stochastic decoding** → **greedy / temp 0** (the biggest single unlock; residual
   MoE/server variance still needs a few trials).
2. **Tool-call path divergence** → **task granularity** (small single-file edits have
   far fewer branch points than "implement this module from a stub").
3. **Partial-credit cliffs** → **rubric decomposition** (one criterion per test
   method → variance of a mean of K Bernoullis, not one ±1.0 step). Already shipped.
4. **Floor/ceiling tasks** (always-0 or always-1) carry zero lift-signal but cost a
   trial → **pre-screen to the headroom band** (baseline mean ≈ 0.3–0.7).

**Unexploited knob: paired/within-task comparison.** The gate compared *means of
independent runs*; pairing each arm on the *same frozen task + seed* differences out
the per-task difficulty term (the dominant variance component).

**Power intuition (paired, tasks as the replication unit):** detect mean lift Δ at
~80% power needs Δ ≳ 2.8·σ/√(M·N). Today σ ≈ 0.3 ⇒ Δ=0.05 needs ~280 task-trials
(infeasible). Drive σ→0.10 (temp-0 + rubric + paired + headroom-screen, *combined*)
⇒ ~31 task-trials = **M=15 tasks × N=2** (feasible). **Target ≈ 3× SD reduction
(0.3→0.10).** No single knob gets there; the gate failed because it had only the
rubric knob (temp-default, unpaired, floor/ceiling tasks in the set).

## 2. Candidate task families

Hard priors from the optimization verdict: specialized coding subagents don't beat a
generalist; routing demoted; rich feedback beats scalar; cross-model memory transfer
degrades. The family measures lift from a **context/harness** intervention (ACE
memory, or a router decision), **not** worker-prompt tuning. Benchmarks are a guard,
not the target.

- **Option A — in-repo single-fault bug-fix microtasks (RECOMMENDED).** Take a repo
  at a commit, inject one well-characterized bug (or revert a real one-commit fix);
  freeze repo-at-commit as `workspace/`, the area's tests as hidden `grader/`, the
  issue as `prompt.txt`. **Headroom is tunable by construction** (pick bug difficulty
  + pre-screen to the 0.3–0.7 band) — the only option where you *control* headroom.
  Low variance (single-fault in-repo edit = few branch points; surrounding code gives
  a pattern; many tests → fine rubric). Freezable natively (`freeze-task.sh`).
  Cheapest source: revert N real one-commit bug-fixes from a repo we dogfood (this
  repo, kypp) — the reverted fix's test is the grader, the commit message ≈ the issue.
  Maps directly to `session score --rubric` (one `NAME :: cargo test x` / `pytest -k`
  per test).
- **Option B — date-gated SWE-rebench via Harbor.** The *destination*, not the start:
  real but often too-much headroom, and full-repo long-horizon = maximal variance
  (the opposite of the granularity knob). Defer until A proves the rig sees a lift.
- **Option C — synthetic with known-difficulty knobs.** Not a bakeoff family
  (optimizer wins on toy tasks don't transfer — the exact false signal the verdict
  warns against), but the **ideal substrate for the §5 sanity check** because
  difficulty is a dial. (Built: `gen-sensitivity-tasks.py`.)

**Ready tooling for the bakeoff family** — the rig already ships importers that
produce the hidden-grader task layout, so curating Option-A/B is *using* these,
not hand-writing tasks: `import-aider-polyglot.py` (Exercism-derived, less
contaminated, stdlib-unittest host-graded = light + agentic — the better A/B set,
the one the gate used), `import-swebench.py` (the real **in-repo** slice =
Option-A; in-sandbox grade + `--grader-egress` to pypi; **switch it to date-gated
SWE-rebench** — SWE-bench-Lite is contaminated, sanity-only), `import-humaneval.py`
(contaminated + weak tests → harness-mechanism only). Freeze a materialized set
into the 3 splits with **`freeze-split.sh <tasks-root> <set> [train:val:test]`** —
a stable-hash partition (a task keeps its split for life → no train↔test leakage
when the set grows) over `freeze-task.sh`. Gated behind the §5 verdict: don't
invest in the real family until the rig proves it can see a planted lift.

**Recommendation:** start with **A**, sourced by reverting real one-commit fixes; use
**C** only for the §5 sanity check; keep **B** as the post-validation generalization
slice.

## 3. Freeze mechanism (discipline, not new code)

- **Content-address:** `freeze-task.sh <task-dir> <set> <split> [id]` →
  `pillbox push --bookmark <set>/<split>/<id>` over `workspace/ + grader/ +
  prompt.txt`; the rustic snapshot captures all three atomically ⇒ the handle is a
  content address over (tree + verifier + prompt). "A changed grader changes the
  score." `run-task.sh` rehydrates by bookmark so every arm starts byte-identical.
- **Three splits, not two:** the gate's "held-out" was really a *validation* set
  (candidates were selected on it ⇒ selection pressure flatters the optimizer).
  `train/` (distill from), `val/` (candidate selection), **`test/` (locked, scored
  once, never selected on)** — the only number that counts.
- **Contamination guards:** hidden grader copied in *after* the turn; **per-criterion
  grader feedback is an offline reflector gradient, NEVER a run-time-injectable
  artifact** (kypp leaked hidden test-method names into a shared claim → fed them back
  to the agent on retry; fixed `4c62a3d` — any memory/ACE arm must redact test
  names + abs paths from anything prepended to a worker prompt); inject-your-own bugs
  (A) dodge benchmark contamination; date-gate the B slice; the **final scored pass
  must be a blind grader** (no feedback re-injection — `rubric-loop.sh` is "hidden
  files, visible feedback", fine for an interactive arm, not for the final score).

## 4. Metric — cost-adjusted quality, paired

Both terms come straight out of the §0 log; no new substrate.

- **Quality:** the rubric fraction from the `Scored` event — read structurally via
  `session score … --json` (`{score, passed, criteria}`) or `session log --type
  scored --last`. The `criteria[]` give the decomposed signal + the reflector's
  textual gradient.
- **Cost:** sum the `Usage` events (`session log --type usage`: input/output/cache
  tokens per `message_id`; `model` from the correlated `MessageEnd`), apply per-model
  pricing ⇒ $/rollout. **Needs a small cost-summer script.**
- **Combine:** report **both** — a Pareto (cost, quality) view per arm, plus a
  scalarization `quality − λ·cost_normalized` for ranking (λ chosen so
  frontier-quality-at-fraction-cost wins; surfaces that an always-escalating cascade
  *loses* on cost).
- **Compare paired-by-task:** statistic = per-task difference d(t) = q̄(arm,t) −
  q̄(baseline,t), aggregated over tasks; **bootstrap 95% CI over tasks**. Verdict =
  "build the harness" only if the CI on the **test** split excludes 0 with Δ ≳ 0.05.
  Emit a **durable manifest** (model, temp, seeds, frozen bookmark handles, per-task
  per-trial score+cost, the CI) as one resumable record — the gate's biggest
  self-criticism was "no seeds, manifests, CIs, locked test split, resumable records."

## 5. The cheapest experiment that de-risks everything — the gate-before-the-gate

**Do NOT run a 3-arm bakeoff next.** Run a **rig-sensitivity check** first: can the
rig detect a *known, injected* lift at a feasible trial count? Three prior runs prove
it currently can't, and a fourth bakeoff without this repeats the mistake.

1. **Family:** 8–12 Option-A microtasks (or Option-C synthetic if curation is slow —
   acceptable here since we're testing the *rig*, not claiming transfer). Freeze under
   `rigcheck/test/<id>`.
2. **Two arms where B is engineered better by a known margin** — not a real optimizer,
   a **planted oracle hint**: A = prompt only; B = prompt + a small *true* hint that
   strictly helps (e.g. "the bug is in `<file>`" — what a perfect router/memory would
   surface). Injects a lift you expect to be positive and roughly sized.
3. **All variance controls on:** temp 0; fine rubric; pre-screen to 0.3–0.7;
   paired-by-task; N=2 then N=3.
4. **Compute** paired mean d(t) + bootstrap CI **and** the empirical per-task
   within-condition SD σ̂ (the number that matters — did σ drop to ≈0.10?).

**Success criterion (the gate on the gate):** σ̂ ≲ 0.10 per task **and** the planted
lift's CI excludes 0 at N≤3 on ≤12 tasks. If both hold → proceed to the real 3-arm
bakeoff (baseline / ACE-runtime-context / GEPA-text-fed) on a *separate locked* test
split, same controls (ACE favored by the verdict). If σ̂ stays high or the planted
lift is invisible → **stop; do not run the bakeoff** — iterate on the rig (local-model
parallelism for more trials, or smaller-granularity tasks), not the optimizer. Cost:
~12 tasks × 2 arms × 3 trials ≈ 72 rollouts, one model, hours.

### RESULT (live run, 2026-06-07) — PASSES literally, but too clean to be informative

Ran live on codesigned libkrun + `pillbox-runner:l7` + opencode `zai-coding-plan/glm-4.5-air`
(12 tasks × baseline/oracle × 2 trials @ temp-0): **σ̂=0.0, mean_d=0.25 (=the planted
lift exactly), CI=[0.25,0.25], `sensitive:true`** — every baseline cell 0.75 (missed
the empty contract), every oracle 1.0, both trials identical. So: the rig detects a
known lift, the stats machinery is sound, and the whole pipeline runs end-to-end. **But
the result clears the bar so cleanly it punts the hard questions**, which the synthetic
family is too clean to answer:
- **σ̂=0.0 is the floor, not the regime that mattered.** These pure-function microtasks
  have ~zero agentic branch points; they don't exercise the path-divergence variance
  that killed the three prior real runs. Validates the *rig*, not real-task variance.
- **temp-0's effect isn't isolated** — the tasks are deterministic even at default temp,
  so same-condition determinism is *consistent with* temp-0 but doesn't prove it tames
  variance (none to tame here).
- **Cost denominator empty** — `pb_usage` found **no `usage` events** in the
  libkrun-opencode §0 log (all cells $0); the cost-adjusted metric can't be computed on
  this path (the §0 producer-fragmentation gap — opencode's SSE→§0 mapper doesn't emit
  `Usage`, or `wait-idle` doesn't drain it).

**The question moves from "can the rig measure lift?" (✅ yes) to two still-open follow-ups
before the bakeoff:** (a) re-run this harness on a small **Option-A real-bug set** (importers
+ `freeze-split.sh` are built) to measure σ̂ in the agentic regime; (b) fix the
libkrun-opencode **usage producer** so the cost half populates. The synthetic pass is the
green light to invest in (a).

### RESULT (a) — agentic-regime σ̂ on the toolz real-bug family (2026-06-07): the stop-branch fires

Ran the same harness on the 5 toolz real-bug tasks (`gen-toolz-tasks.py`), 2 replicate
arms × 3 trials = 30 cells @ temp-0, glm-4.5-air. **σ̂ = 0.35** (target ≲0.10), mean_d=0.0
(replicates → no spurious lift, good), CI = [-0.53, +0.47] (straddles 0 hugely),
`sensitive: false`. **Every task is flaky:** within-condition trial triples like
`[1,0,0]`, `[1,0,1]`, `[1,1,1]` vs `[0,0,0]` for the *same* condition — per-task pass
rates 1/6–4/6 with within-cell SDs ~0.37–0.50 (near-maximal Bernoulli noise). glm is
**non-deterministic per task even at temp-0** on real multi-file bug-fixes.

This is the doc's **§5 stop-branch**: the synthetic σ̂=0 was the floor (deterministic
single-function tasks); the moment the task is real-codebase + agentic, σ̂ jumps to ~0.35
and **temp-0 doesn't touch it** — confirming the variance that killed the three prior runs
is **agentic path-divergence** (or a hosted-MoE not actually greedy at temp-0), NOT decoding
temperature. Power: detecting a plausible Δ≈0.10 at σ=0.35 needs ≈100 task-trials/arm —
infeasible, and per-task flakiness makes even that shaky. **Conclusion: do NOT build the
GEPA/meta-harness on this regime+model** — lift is unmeasurable here. The rig is sound; the
*regime* can't measure lift. Iterate on the regime, not the optimizer: a genuinely
deterministic worker (a local greedy model, not a hosted MoE), or far more trials, or accept
the §0 substrate as the durable win and shelve compile-time optimization. (Caveats: N=3×5 is
small but σ̂≈0.35 w/ SDs~0.5 is unambiguous; one model; temp-0's effect still unisolated —
but if it took and σ̂ is still 0.35, that's the finding either way.)

### RESULT (b) — the deterministic-worker retry, LOCAL greedy qwen (2026-06-10): σ̂ bar PASSES on toolz; headroom and variance anti-correlate across families

Reran RESULT (a)'s exact protocol (5 toolz tasks × 2 replicate arms × 3 trials @ temp-0)
with the worker swapped to **local `ollama/qwen3.6:35b-a3b-coding-nvfp4`** via the libkrun
local-model forward (`PILLBOX_LOCAL_MODEL_PORT=11434`, 042648c). Same rig, same tasks, same
stats — only the worker changed.

**Leg A (toolz): σ̂ = 0.0577 ≤ 0.10 — the bar PASSES.** mean_d = −0.067, CI [−0.2, 0.0]
(replicates → includes 0 ✓). 29/30 trials scored 1.0; 9 of 10 cells perfectly deterministic;
the single flip was `sliding_window` oracle `[0,1,1]`. vs RESULT (a)'s σ̂ = 0.346 on the
identical protocol: a **6× variance reduction from the worker swap alone**. The deterministic-
worker hypothesis is confirmed — the σ̂ wall was the hosted MoE, not the agentic regime per se.
But: qwen sweeps the family (mean 0.97 vs glm's ~0.5), so **toolz has zero headroom for this
worker** — nothing to mine, nothing to lift.

**Leg B (aider graded greenfield: ap_pov / ap_connect / ap_pig_latin, same replicate
protocol): σ̂ = 0.154 — above the bar** (though 2.2× better than hosted). Per-task triples:
pov all-0 (deterministic floor), connect baseline `[0,0,0]` / oracle `[0,0.6,0.6]`,
pig_latin baseline `[0,1,1]` / oracle `[1,1,1]`. Two named suspects, both testable:
1. **Prefix-cache cold/warm divergence** — in 4 of the variable cells the *first* trial is
   the outlier and later trials agree (`[0,1,1]`, `[0,0.6,0.6]`, …): consistent with
   ollama's fresh-prefill vs cached-prefill numeric paths diverging at a near-tie token.
2. **Turn-cap guillotine** — MAX_WAIT=600 truncates long greenfield turns, converting small
   timing/verbosity jitter into large score variance (score = where the axe fell).
Also a free demonstration of why the CI gate exists: the two replicate arms differ only by
two prepended newlines, yet mean_d = +0.244 on n=3 tasks — a fake "lift" from prompt
perturbation + small N, correctly refused (`sensitive: false`).

**The structural finding: for a fixed worker, σ̂ and headroom anti-correlate across
families.** Where the worker is strong (toolz) it is near-deterministic but saturated; where
tasks leave gradient (aider greenfield) variance returns. The de-risking prerequisite
(σ̂ ≤ 0.10 AND partial-credit headroom on the SAME family) is therefore **half-met**: the
worker-swap unblock works, the joint regime is not yet exhibited. Knob test (in flight):
rerun leg B at MAX_WAIT=1800 with a recorded cold warm-up trial per task — if warm-trial
σ̂ ≤ 0.10 with means strictly inside (0,1), the prerequisite is met on aider and the
self-harness arm (docs/ghost-self-harness.md §6) unblocks; if not, the gap is a family of
intermediate difficulty (toolz-shaped surgical fixes, but harder — e.g. multi-fault or
cross-module variants).

Ops notes from the run (substrate, not stats): two transient libkrun bring-up hangs
(~6% of ~45 boots; `run --json` stuck pre-reparent, no session record — kill + relaunch;
a batch medic that kills `pillbox run --json` older than 20 min self-heals it), and
`materialize_rootfs` leaks the `docker rm -f` container id to stdout on a cold cache,
corrupting `run --json` (fix queued: capture the Command output).

## Appendix — built vs. needed

| Need | Status |
|---|---|
| Content-addressed freeze (workspace+grader+prompt) | **built** — `freeze-task.sh`, `push --bookmark`/`pull` |
| Verifiable graded reward + per-criterion feedback | **built** — `session score --rubric`, `Scored`/`Criterion` |
| Drive spine (run→send→wait-idle) | **built** — `lib.sh`, `run-task.sh` |
| Token/cost from §0 log | **built** — `pb_usage` in `scripts/eval/lib.sh` folds `session log --type usage` → tokens + `$cost` (per-1M-token env prices); the log's emission-time wire/native precedence means no double-count |
| Temp-0 worker decoding | **needs wiring** (`MODEL` override exists; set provider temp 0) |
| Paired comparison + bootstrap CIs + σ̂ | **built** — `scripts/eval/paired-stats.py` (paired per-task diff, seeded bootstrap CI over tasks, pooled within-cell σ̂, `--self-test` recovers a planted lift / refuses high-σ + null) |
| Temp-0 (greedy) decoding | **built** — `pillbox run --temperature` (server agents), `TEMPERATURE` env in the rig |
| Sensitivity-check runner (§5) | **built + RUN LIVE (2026-06-07)** — `scripts/eval/sensitivity-check.sh`; verdict `sensitive:true` (σ̂=0.0, lift=0.25, CI excludes 0) on libkrun+opencode. Rig validated; see §5 RESULT for the caveats (synthetic floor, cost denominator empty) + the two follow-ups |
| 3-split (train/val/**test-locked**) | **built** — `freeze-split.sh` stable-hash-partitions a task-dir set into train/val/test bookmarks (stable under growth; ratios approximate for small N, printed counts authoritative) |
| Headroom pre-screen | **not built** — one baseline pass dropping floor/ceiling tasks |
| Sensitivity-check task family (Option-C synthetic) | **built** — `scripts/eval/gen-sensitivity-tasks.py` emits 12 microtasks + a uniform `oracle.md`; each prompt OMITS the same arbitrary "empty→-1" contract the hidden rubric checks → a structurally-guaranteed, uniform planted lift (validated: correct→4/4, baseline→3/4, so lift = 1 criterion/task). `--self-test` certifies it. Generated under `scripts/eval/sensitivity-tasks/` |
| Option-A real-bug task family (agentic) | **built** — `scripts/eval/gen-toolz-tasks.py`: 5 single-fault bugs injected into REAL `toolz` functions (off-by-one in take/drop/take_nth, tail slice, sliding_window), graded by toolz's REAL pytest suite (hidden). Real codebase + agentic multi-module navigation + dep-light (pure-Python; graded via `uv run --with pytest`, no pip-per-grade). `--self-test` validates each (clean→test passes, bugged→fails) + end-to-end grade-sim confirmed. Generated on demand (bulky); `freeze-split.sh <out> toolz` to freeze. The agentic-regime σ̂ measurement runs on this (next) |
| Grader-leak redaction in any memory arm | **fixed in kypp** (`4c62a3d`); re-verify new injection paths |

The substrate is done. What's missing is **discipline** (3 splits, headroom screen,
temp-0, pairing) + a **durable stats artifact** — and, before any of it, the **§5
sensitivity check** proving the rig can see a planted lift. That check is the gate
before the gate.

## 6. Second deep-research pass (2026-06-08) — arm deltas + a rig-validity gap

A literature-only pressure-test (101 agents, 19 sources, 22/25 claims confirmed;
`tasks/wn1e3j40m.output`) independently re-derived this gate and changed three priors.
None reverse the parked decision — the recommended 3-arm bakeoff was already run here
(§5: σ̂=0.346 on hosted glm → lift unmeasurable); the blocker remains the variance
**regime**, not a missing harness.

- **Downgrade frontier→cheap model is now REFUTED, not just unproven.** Both supporting
  claims died in 3-vote verification (Databricks "90× cheaper, beat Opus"; GEPA
  coding-skills cross-harness transfer). Shopify's downgrade was classification/extraction;
  the cross-model-transfer premise has zero surviving support for coding. Drop the arm.
- **ACE is now a mandatory arm, and our rig under-represents it.** ACE (arXiv 2510.04618)
  beats GEPA +11.9% on AppWorld and +14.8% with NO labels (execution feedback only), and
  names GEPA's "brevity bias" as a failure mode for program synthesis / multi-step agents.
  **`gate.py`'s `ace` arm is a STATIC-PLAYBOOK PREPEND, not real ACE** (runtime context
  evolution: a Reflector that grows the playbook from execution feedback via incremental,
  non-collapsing deltas). A null "ace" result in the current rig must NOT be read as "ACE
  doesn't help." Upgrading to a true evolving-context arm is **gated behind the
  deterministic-worker retry** — don't build it while σ̂ is unmeasurable (would be
  polishing a rig whose blocker is variance, not arm fidelity).
- **Reward integrity is the quantified real risk.** Binary test-pass is Goodhart-leaky:
  21.8% (Claude-3.7) – 33% (GPT-4o) of patches passing model-generated tests fail hidden
  tests, and refining against those tests makes overfitting WORSE (arXiv 2511.16858). The
  hidden-grader + `score`-vs-`done` separation is the correct defense; the rubric (rich
  per-criterion textual feedback) is what justifies GEPA over a scalar optimizer at all.
