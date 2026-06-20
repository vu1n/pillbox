# Plan: Ghost v1 — interactive orchestrator over verified worker loops

> **Status (2026-06-14):** GHOST-001 ✅ merged (libkrun boot-script fix, on `main`).
> GHOST-002 ✅ merged (#58): `pillbox dispatch` contract (CLI + types + docs);
> handler was a stub. GHOST-003 ✅ merged (#60 → `main`): the fork/score/select
> **loop** — subprocess orchestration (self-exec + `--json`, NOT in-process — the
> handlers are private `Result<()>` fns), `WorkerDriver` trait + `CliDriver`,
> distilled-retry feedback, turn-timeout + partial-fork + non-fatal-pull
> robustness; 11 unit tests. Verb name settled (`pillbox dispatch`, not `swarm`);
> `--temperature` in v1. GHOST-004 ✅ merged (#61 → `main`): live e2e
> smoke (`scripts/smoke/dispatch.sh`, 2/2 workers scored + winner pulled on real
> libkrun VMs) — surfaced + fixed two bugs unit tests couldn't (send-driven first
> turn for server agents; `--json` stdout purity). Dispatch is **libkrun-only**
> today (grader resolves the live workspace a libkrun-only way; docker deferred).
> GHOST-006 ✅ merged (#62 → `main`): the σ̂-segmentation **harness**
> (`scripts/eval/segmentation/run.sh` + `segments/ap_pov/` + README). Two arms —
> monolithic vs a fresh-session-per-segment chain (horizon RESET at each
> checkpoint) — scored on the SAME full rubric; emits JSONL → `paired-stats.py`
> + an in-script per-arm σ̂ summary (the pooled number hides the contrast).
> `--dry-run` gate green. It's dispatch's segmentation SIBLING (shares the
> primitives, doesn't shell `pillbox dispatch` — dispatch can't inject the
> hidden grader the eval family needs; `SEG_K>1` = the best-of-k follow-up).
> **GHOST-007 ✅ POSITIVE (#63 → `main`):** σ̂ 0.467 → 0.000 (monolithic →
> segmented), mean 0.42 → 1.00 on ap_pov/glm-5.1/n=10. The variance frame HOLDS
> — segmentation cuts σ̂ decisively on the first keystone measurement. Verdict +
> caveats in `docs/optimization-gate.md`. **HARDENING ✅ COMPLETE (H1/H2/H3/H4 all
> merged) — the keystone holds across every cut.** H3 launch-retry (#64), H1
> multi-task (#66: 3 tasks, lift +0.41 CI [0.25,0.53] excludes 0 — generalizes),
> H2 retry-isolation (#68: SEG_RETRIES=0 still cuts σ̂ → retry not the driver),
> **H4 reset-vs-scope (#74: the mechanism is FOCUSED SCOPE, not session reset —
> `chained` one-session arm captures 98% of the σ̂-cut + 105% of the lift;
> horizon-reset-on-top adds nothing).** Net dispatch-design result: **in-session
> focused-prompt chaining + per-checkpoint verification IS the segmentation lever;
> fork-per-segment is the best-of-k DIVERSITY lever (a separate axis).** **Next:
> H5** = cross-model robustness (the σ̂-cut beyond glm-5.1) — gated on the
> deepseek/kimi pool (acquired direct, cost). Only after H5 do the compounding
> tasks (GHOST-005/008/010-012 + dispatch best-of-k × segmentation) pay off.
> (Cross-vendor pool also feeds GHOST-011 judge-mode/best-of-k diversity.)
> GHOST-005 (orchestrator skill) + GHOST-008 (harvest) unblocked, lower-stakes. **Adjacent workstreams** GHOST-010 (Ripple
> context-pack), GHOST-011 (dispatch judge-mode), GHOST-012 (multi-actor §0) added
> 2026-06-14 — aligned but **off the v1 critical path** (gated behind the σ̂
> keystone for priority); see the section at the bottom.

Ghost v1 = the chat-as-orchestrator design: an interactive agent spins up k forked,
rubric-verified worker sessions via a new `pillbox dispatch` verb, on libkrun first.
Five streams: (1) the libkrun cmdline-ASCII blocker (fix already WIP in the working
tree — finish + live-verify), (2) the dispatch glue verb, (3) the skill + rubric
library that teaches an interactive agent to use it, (4) the σ̂ segmentation
experiment (the keystone assumption test), (5) the harvest pipeline (failed
sessions → frozen tasks). **Critical path:** GHOST-002 → 003 → 004 → 007 (contract
→ impl → live verify → experiment verdict), 4 tasks long; GHOST-001 joins at 004.

Naming note: the verb is provisionally `pillbox dispatch` (avoids colliding with
"swarm memory" = kypp vocabulary). Rename is a find-replace before GHOST-003 lands,
not after.

## GHOST-001 — Finish + live-verify the libkrun boot-script fix (WIP in tree)

The static-cmdline → `.pillbox-boot.sh` fix is already implemented uncommitted in
`src/sandbox/libkrun/` (bootstrap_exec / env_exports / static_child_env + unit
tests + rootfs cache-key fix). Remaining work: fmt + clippy (default AND
--features libkrun), unit tests, lk-build + live smoke, and the two live cases
that motivated the fix — a multi-line/unicode seeded prompt and a `--memory`
briefing run — then commit. This unblocks dispatch prompts on libkrun.

```yaml
id: GHOST-001
task_type: bug_fix
archetype: rust-systems
depends_on: []
footprint:
  modifies:
    - "src/sandbox/libkrun/session.rs::*"
    - "src/sandbox/libkrun/mod.rs::vmm_child_main"
    - "src/sandbox/libkrun/mod.rs::materialize_rootfs"
    - "src/sandbox/libkrun/mod.rs::docker_image_id"
    - "src/sandbox/libkrun/mod.rs::rootfs_cache_key"
    - "runner/Dockerfile"
gate: "cargo test --features libkrun passes; scripts/smoke/libkrun.sh green; a libkrun run with a multi-line unicode prompt AND a --memory briefing run both boot (no InvalidAscii) — verified live"
assumptions:
  - "WIP diff in working tree is the intended design (static cmdline + boot script in creds share); finish it, don't restart"
  - "runner/Dockerfile diff (6 lines) belongs to this change; if unrelated, split commit"
```

## GHOST-002 — Dispatch contract: CLI surface, types, JSON verdict schema

Contract-first. Declare `pillbox dispatch` in the clap tree (top-level Command
variant in src/main.rs + args struct), create `src/commands/dispatch.rs` with the
core types (DispatchOpts: bookmark, k, rubric path, retries, agent/model,
--memory passthrough, --json; WorkerOutcome; DispatchVerdict = winner session id +
per-worker scores + pulled-workspace path) and a stub handler, and write
`docs/dispatch.md` with the flag table + verdict JSON schema + exit codes
(0 winner found / 1 all workers failed / 2 usage). Everything downstream programs
against this file pair.

```yaml
id: GHOST-002
task_type: feature
archetype: rust-cli
depends_on: []
footprint:
  modifies:
    - "src/main.rs::Command"
    - "src/main.rs::main"
    - "src/commands/mod.rs::*"
  creates:
    - "src/commands/dispatch.rs"
    - "docs/dispatch.md"
gate: "cargo check passes both feature sets; docs/dispatch.md documents every flag in the clap declaration (names match 1:1) and the verdict JSON schema with a version field"
```

## GHOST-003 — Dispatch core loop: fork-k, drive, score, select-best

Implement the loop in `src/commands/dispatch.rs` against the GHOST-002 contract:
resolve bookmark → launch k detached worker sessions (`run --from-bookmark
--detach --json` path, libkrun or docker backend) → per worker: wait-idle → `score
--rubric --json` → on fail with retries left, `session send` the failing criteria
as feedback and loop → select max score (ties: fewer retries, then earliest) →
`session pull` the winner → emit DispatchVerdict. Reuse the existing handlers in
src/commands/session/mod.rs as in-process calls, not subprocess shell-outs.
Losers are left for `session rm`/prune (recorded in the verdict), not auto-killed.

> **Research refinement (2026-06-13, SOTA — arXiv 2604.16529 Parallel-Distill-Refine):**
> the retry feedback should be a **distilled failure summary** (what was
> hypothesized, what progressed, why it failed), NOT the raw rubric diff — raw
> execution logs are too noisy for the model to act on; conditioning the next
> attempt on a distilled summary is the measured-better form (and it's exactly
> our signal-not-content / kypp-distill discipline applied inside the loop). The
> rubric criteria stay the *gate*; the distilled summary is the *prompt*. Doesn't
> touch the CLI/JSON contract — it's how the loop composes the next `session send`.
> Also: best-of-k=3 is the validated default for short horizons (k=3–5 saturates);
> segmentation is the higher-leverage lever, best-of-k mops up the residual.

```yaml
id: GHOST-003
task_type: feature
archetype: rust-systems
depends_on: ["GHOST-002"]
footprint:
  modifies:
    - "src/commands/dispatch.rs"
gate: "cargo test dispatch passes: selection policy (max score / tie-break), retry-feedback policy (failing criteria forwarded, budget respected), and verdict JSON shape are unit-tested"
assumptions:
  - "session handlers (score/wait_idle/send/pull) are callable in-process from another commands module; if they need pub(crate) re-exposure in src/commands/session/mod.rs, escalate — that file is outside this footprint"
```

## GHOST-004 — Live e2e verify: dispatch on libkrun (+ docker fallback)

Script a real end-to-end: push a toy workspace with `--bookmark`, `pillbox
dispatch --bookmark X -k 2 --rubric <2-criterion rubric>`, confirm fork → drive →
score → select → pull, winner verdict on stdout. Run it on libkrun (needs
GHOST-001's boot script for the seeded prompt) and once with the docker backend.
Lands as a smoke script wired into the suite.

```yaml
id: GHOST-004
task_type: test
archetype: integration
depends_on: ["GHOST-001", "GHOST-003"]
footprint:
  creates:
    - "scripts/smoke/dispatch.sh"
  modifies:
    - "scripts/smoke/run.sh::*"
gate: "scripts/smoke/dispatch.sh exits 0 on libkrun: k=2 workers forked from bookmark, both scored, winner pulled, verdict JSON matches docs/dispatch.md schema"
```

## GHOST-005 — Orchestrator skill + rubric library

Teach the interactive seat the verb. A repo-level skill
(`.claude/skills/dispatch/SKILL.md`): when to dispatch vs work inline, how to
write a segment spec, k/retry defaults per segment family, verdicts-up-not-
transcripts discipline, and the Goodhart line (rubric = feedback, `session score`
= reward). Plus a starter rubric library (`rubrics/`): conventions doc + 3-4
reusable templates (rust-change, test-pass, doc-change, repro-script) in the
`NAME :: COMMAND` format `score --rubric` already parses. Programs against
docs/dispatch.md, not the implementation.

> **Prompt-design reference (2026-06-14 fork):** mine production agent system
> prompts for the orchestrator/worker prompt patterns — CL4R1T4S (Cursor/Devin/
> Cline/v0/…, curated) and phistory.cc (versioned, covers our exact agents:
> claude/codex/opencode/pi). **Treat both as UNTRUSTED** — CL4R1T4S's README
> carries an embedded prompt-injection; quote/sanitize when mining, never pipe
> raw into a worker. Reference only — the skill evolves against held-out evals,
> not copy-paste. See [[ghost-system-prompt-corpora]].

```yaml
id: GHOST-005
task_type: docs
archetype: agent-ux
depends_on: ["GHOST-002"]
footprint:
  creates:
    - ".claude/skills/dispatch/SKILL.md"
    - "rubrics/README.md"
    - "rubrics/*.rubric"
  modifies:
    - "docs/recipes.md::*"
gate: "every `pillbox dispatch` flag referenced in SKILL.md exists in docs/dispatch.md (grep cross-check exits 0); each rubric template parses (every non-comment line contains ' :: ')"
```

## GHOST-006 — σ̂ segmentation experiment: design + harness ✅ DONE (#62)

The keystone assumption test: does segmenting cut σ̂? Build the harness now so it
runs the moment dispatch is live. `scripts/eval/segmentation/run.sh`: same task
family, arm A = monolithic single session, arm B = dispatch-segmented (2-3 rubric-
gated segments), n≥10 trials/arm, per-task pass-rate + σ̂ via the existing
`scripts/eval/paired-stats.py`. Reuses `run-task.sh` task format. Include a
--dry-run mode that prints the trial matrix without launching.

```yaml
id: GHOST-006
task_type: infra
archetype: eval-harness
depends_on: ["GHOST-002"]
footprint:
  creates:
    - "scripts/eval/segmentation/run.sh"
    - "scripts/eval/segmentation/README.md"
gate: "scripts/eval/segmentation/run.sh --dry-run exits 0 and prints both arms × n trials with task ids, segment specs, and rubric paths resolved"
assumptions:
  - "an existing eval task family has segmentation headroom (not saturated toolz); if task selection needs a new multi-fault family, that's a scope change — escalate"
```

## GHOST-007 — σ̂ segmentation experiment: execute + verdict ✅ DONE (#63) — POSITIVE

**Verdict (2026-06-14, `docs/optimization-gate.md`): the variance frame HOLDS.** ap_pov,
glm-5.1, n=10/arm, libkrun: σ̂ **0.467 → 0.000** (monolithic → segmented), mean 0.42 →
1.00, perfect-rate 2/10 → 10/10. Monolithic = textbook long-horizon bistability (5/10
bail, genuine minimal attempts per cost telemetry — not session zeros). Segmentation
collapses the variance AND lifts the mean. Records: `scripts/eval/segmentation/results/`.
**Hardening ✅ COMPLETE — the keystone holds across every cut.** H3 launch-retry +
state-reap (#64/#66); **H1 multi-task** (#66): 3 tasks (dot_dsl/grade_school/pov), pooled
σ̂ 0.212 → 0.026, lift +0.41 CI [0.25,0.53] excludes 0 — GENERALIZES. **H2 retry-isolation**
(#68): SEG_RETRIES=0 still cuts σ̂ (0.251 → 0.037) → retry is NOT the driver, only a
second-order amplifier. **H4 reset-vs-scope** (#74): a 3rd `chained` arm (focused prompts,
ONE session) isolates scope from horizon-reset — **the mechanism is FOCUSED SCOPE**: chained
captures 98% of the σ̂-cut + 105% of the lift, horizon-reset-on-top adds nothing (Δσ̂ −0.003,
lift CI includes 0). On ap_pov (the high-σ̂ task where reset should matter most) chained =
segmented = 1.00±0.00. **Dispatch design settled: in-session focused-prompt chaining +
per-checkpoint verification = the segmentation lever; fork-per-segment = the best-of-k
DIVERSITY lever (separate axis).** **Only H5 left** = cross-model robustness beyond glm-5.1,
gated on the deepseek/kimi pool (acquired direct, cost). Verdicts §H1/§H2/§H4 in
docs/optimization-gate.md; records in `scripts/eval/segmentation/results/`.



Run GHOST-006's harness live, both arms, n≥10 per arm. Record σ̂ and pass-rate per
arm, paired stats, and write the verdict into docs/optimization-gate.md (the
existing gate doc). This is the go/no-go for the whole variance frame: if
segmentation doesn't cut σ̂, ghost-learn's design needs rework before any further
build.

> **Research framing (2026-06-13, arXiv 2603.29231 — converts this from a guess
> to a well-posed measurement):** the keystone hypothesis is theory-backed —
> success decays p^n with horizon and variance grows faster than the mean, so
> "σ̂ scales with turn horizon" is the *expected* shape, not an artifact; the
> claimed decomposition lift is 40–60%, **"depending on checkpoint quality."**
> Two confounders to control or the experiment measures noise, not segmentation:
> (1) **verifier quality is a co-variable** — a weak rubric adds latency without
> cutting variance, so a null result could be weak-verifier, not weak-
> segmentation; pin rubric quality and treat it as a measured factor. (2) **the
> task family must have capability headroom** — on a *saturated* family (toolz)
> there's no room for capability-variance to express (this reframes the 6/12
> control: a stronger worker can't move σ̂ on a flat task), so segmentation can't
> show a delta. Use a long/hard-enough family (the planned multi-fault toolz at
> surgical horizon). (3) **calibrate expectations:** SOTA parallel+sequential
> methods buy single-digit to low-double-digit point gains (SWE-Bench +6.7,
> Terminal-Bench +12.2) — a small-but-real lift is success here, not failure.

```yaml
id: GHOST-007
task_type: test
archetype: eval-execution
depends_on: ["GHOST-004", "GHOST-006"]
footprint:
  creates:
    - "scripts/eval/segmentation/results/"
  modifies:
    - "docs/optimization-gate.md::*"
gate: "paired-stats.py emits σ̂ for both arms from ≥10 completed trials each; verdict (segmentation cuts σ̂: yes/no + effect size) recorded in docs/optimization-gate.md"
```

## GHOST-008 — Harvest pipeline: failed session → frozen task

`scripts/eval/harvest-session.sh ID`: read the session record (`session info
--json`), the §0 log (`session log` — prompt, scored event, rubric), and the
result/bookmark snapshot, and emit a frozen-task dir in the format
`scripts/eval/freeze-task.sh` / `run-task.sh` already consume. Failed sessions
become reproducible eval tasks by construction — the headroom engine. Works
against docker sessions today; libkrun sessions once GHOST-001 lands (no code
dependency, so no edge).

```yaml
id: GHOST-008
task_type: infra
archetype: eval-harness
depends_on: []
footprint:
  creates:
    - "scripts/eval/harvest-session.sh"
  modifies:
    - "scripts/eval/README.md::*"
gate: "harvesting a deliberately-failed docker session produces a task dir that scripts/eval/run-task.sh accepts and re-runs (exit 0 on load, task fails as expected)"
```

## GHOST-009 — Surface docs: CLAUDE.md + vnext

Add the `pillbox dispatch` row to CLAUDE.md's command table and a short ghost-v1
section to docs/vnext.md pointing at docs/dispatch.md, the skill, and the
experiment. Last, so it documents what shipped rather than what was planned.

```yaml
id: GHOST-009
task_type: docs
archetype: docs
depends_on: ["GHOST-003"]
footprint:
  modifies:
    - "CLAUDE.md::*"
    - "docs/vnext.md::*"
gate: "every flag in CLAUDE.md's dispatch row appears in `pillbox dispatch --help` output (manual grep cross-check exits 0)"
```

---

# Adjacent workstreams (surfaced 2026-06-14 — Omnigent / Fusion / Ripple discussion)

Three ideas pulled from an adjacent architecture proposal (Omnigent = external
meta-harness UI; OpenRouter Fusion = multi-model judge panel; Ripple = context
compiler). They are **aligned** with the established north stars but mostly
**off the v1 critical path** — the σ̂ keystone (GHOST-007) still gates the whole
variance frame, and these are the layers that compound *around* the verified
worker loop, not prerequisites to it. Each carries the guardrail that keeps it
from drifting into a product (the artifact-not-product decision stands).

**The line that governs all three:** pull the *protocol / primitive*, not the
*surface / product*. We own the substrate (pillbox §0, kypp, execution-grounded
reward); external surfaces (Omnigent, Slack, a web client, Fusion) are clients
of it, not things we rebuild.

## GHOST-010 — Ripple: a typed context-pack compiler (the per-segment briefing)

The one genuinely new primitive from the proposal, and the operationalization of
the settled north star (*the leverage is the context+memory scaffold, not
routing*). Given "agent is about to edit symbol X," compile a **budget-safe,
signatures-only** pack — target signature, callers/callees (signatures only),
related tests, accepted kypp pitfalls/decisions, inferred contracts, and
staleness warnings (anchors that no longer resolve) — instead of a RAG blob.
Composes with the worker loop: **GHOST-003 forks k workers → each briefed by a
Ripple pack**, not a raw dump. Built behind the existing swappable `CodeResolver`
seam (memory `pillbox-ghost-code-grounding`: ripgrep default, AST/canopy
optional-at-scale — do NOT stand up a heavy `codegraph` MCP prematurely). kypp
stays best-effort, never a gate. Non-blocking: dispatch ships with
kypp-briefing-only first; Ripple is the briefing upgrade.

```yaml
id: GHOST-010
task_type: feature
archetype: context-assembly
depends_on: ["GHOST-003"]
footprint:
  creates:
    - "the Ripple context-pack skill/primitive (home TBD — skill vs CLI vs MCP, decide at design)"
gate: "ripple(symbol) returns the typed pack (target/signature/callers/callees/tests/kypp_memory/contracts/staleness) for a real symbol in this repo, signatures-only, under a fixed token budget; a dispatched worker briefed with it edits the symbol correctly"
assumptions:
  - "reuses kypp recall + the CodeResolver seam; no new heavy code-graph dependency in v1"
  - "post-keystone polish, but the typed pack shape can be prototyped against GHOST-003 early"
```

## GHOST-011 — Dispatch judge-mode: the second selection lane (Fusion-shaped)

Today dispatch selection is single-mode: execution-grounded rubric → max score —
the *stronger* primitive, and it stays **primary** (don't regress toward
LLM-judge where a verifier exists). But for segments with **no rubric**
(design/judgment) and for a **pre-merge Goodhart guard** on the verifier-selected
winner, add a second mode: a **cross-vendor** panel → **structured** critique
(consensus / contradictions / gaps / blind-spots), the no-verifier analog of
per-criterion rubric feedback. Cross-vendor matters: a model grading its own
family is biased (same instinct as vuln-triage's perspective-diverse refuters).
The highest-value use is **both on one winner** — verifier-mode picks it
objectively, judge-mode critiques it before it's trusted (a winner that gamed the
rubric gets caught). Reuse the in-house judge-panel / adversarial-verify patterns
(the Workflow tooling already does this); Fusion is one possible backend, not a
dependency.

```yaml
id: GHOST-011
task_type: feature
archetype: rust-systems
depends_on: ["GHOST-003"]
footprint:
  modifies:
    - "src/commands/dispatch.rs (a second, opt-in selection/critique mode)"
    - "docs/dispatch.md::* (judge-mode + its structured output)"
gate: "dispatch can run judge-mode on a no-rubric segment and emit a structured critique (consensus/contradictions/gaps), AND run it as a pre-merge pass on a verifier-selected winner; rubric mode stays the default and unchanged"
assumptions:
  - "execution-grounded rubric remains primary; judge-mode is fallback + pre-merge guard, never the default for verifiable work"
  - "cross-vendor panel needs >1 model auth available; degrade to single-vendor with a logged note when not"
```

## GHOST-012 — §0 as a first-class multi-actor surface (protocol, not frontend)

The valuable pull from Omnigent's interface is **not** the phone/desktop app (a
product, commoditized, artifact-not-product collision) — it's the requirement
that one live session be readable + drivable by **multiple identified actors from
anywhere**, with external surfaces (Omnigent, Slack, a thin web client) as
*clients* of our §0 protocol. We already have the primitives (`session send` /
`subscribe` / `watch` / `annotate` with actor attribution); what's missing is the
substrate to make them multi-actor. Taking this seriously is a **forcing
function**: multiple writers into one session require a single sequencer, so this
is the lever that finally resolves the **seq-authority fault line** (memory
`pillbox-architecture-review-2026-06`) — it's a §0-authority decision, not a UI
task. Graduated: a small **local multiplayer demo** (persistent `subscribe`
daemon, bind beyond localhost + an auth token, a thin §0-JSON-WS web client) is
dogfoodable now; the polished multi-surface/cross-network version is the
**managed-tier DO-as-§0-gateway** (memory `pillbox-managed-tier-do-gateway`),
which stays **non-P0** — let Omnigent *be* that surface by speaking the protocol.

```yaml
id: GHOST-012
task_type: spike
archetype: substrate
depends_on: []
footprint:
  modifies:
    - "the §0 surface (session subscribe/annotate) — resident reachable gateway + multi-actor identity"
gate: "two distinct actors (distinct identity tokens) read AND write one live session over §0 in seq order with correct attribution — the local multiplayer demo; the seq-authority model is decided + written down before any managed-gateway build"
assumptions:
  - "do NOT build a frontend/app — external surfaces are clients of the protocol"
  - "the resident/managed gateway is non-P0 (artifact-not-product); the local demo + the seq-authority decision are the in-scope parts"
  - "orthogonal to the σ̂/dispatch critical path — a surface/circulation workstream, gated behind the keystone for priority"
```
