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
  difficulty is a dial.

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

## Appendix — built vs. needed

| Need | Status |
|---|---|
| Content-addressed freeze (workspace+grader+prompt) | **built** — `freeze-task.sh`, `push --bookmark`/`pull` |
| Verifiable graded reward + per-criterion feedback | **built** — `session score --rubric`, `Scored`/`Criterion` |
| Drive spine (run→send→wait-idle) | **built** — `lib.sh`, `run-task.sh` |
| Token/cost from §0 log | **data present** (`Usage` event) — needs a small cost-summer |
| Temp-0 worker decoding | **needs wiring** (`MODEL` override exists; set provider temp 0) |
| Paired comparison + bootstrap CIs + manifest | **not built** — replace `gate.py`'s mean-of-independent-runs (small Python tool) |
| 3-split (train/val/**test-locked**) | **convention** — enforce in the freeze step |
| Headroom pre-screen | **not built** — one baseline pass dropping floor/ceiling tasks |
| Option-A microtask curation (revert real fixes) | **not built** — the main human cost |
| Grader-leak redaction in any memory arm | **fixed in kypp** (`4c62a3d`); re-verify new injection paths |

The substrate is done. What's missing is **discipline** (3 splits, headroom screen,
temp-0, pairing) + a **durable stats artifact** — and, before any of it, the **§5
sensitivity check** proving the rig can see a planted lift. That check is the gate
before the gate.
