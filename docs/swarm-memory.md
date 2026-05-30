# Swarm memory + the optimization loops

Status: design / **speculative** (2026-05-30). A **consumer** of the pillbox
contract, **not** pillbox core — same altitude as the orchestrator /
optimization layer ([vnext.md](./vnext.md) §Optimization, CUT from this repo).
This doc records the design so the *substrate primitives it needs* are tracked;
it is gated on the smoke test below proving value first.

## Prove it before you build it (zero new pillbox code)

> Run the **run-time loop only**: `pillbox run --mcp memory=http://localhost:7777`
> against a self-hosted **mem0/OpenMemory** (already the literal example in
> [shared-mcp.md](./shared-mcp.md)), hand-write 3–5 playbook bullets for one
> task class, and **A/B with-vs-without on ~20 real tasks from the existing
> event log**. Measure pass@1 delta. **If retrieved memory doesn't beat the
> no-memory baseline on *your* tasks, neither loop is worth building.** Defer
> GEPA entirely until this is green *and* a verifiable-reward harness exists.

Everything below is the design that earns its place only after that.

## Two loops, one artifact, one validation harness

Both are fed by the per-session append-only **event log** (the durable spine),
both live **outside** pillbox, and they produce **the same object class** —
abstracted, weight-free, natural-language judgment text gated by a verifiable
reward + rich textual feedback. ACE's authors confirm the two compose on the
same context. So build **one artifact economy and one validation harness**, not
two pipelines.

| | Run-time loop (ACE-style swarm memory) | Compile-time loop (GEPA / `optimize_anything`) |
|---|---|---|
| When | online, incremental | offline, batch — **build last** |
| Consumes | successful completed sessions + transcripts | the log as a trace/ASI corpus + a **verifiable score** per rollout |
| Produces | itemized playbook bullets, served via MCP | an evolved scaffold/profile (`dict[str,str]` of judgment fields), Pareto frontier |
| Cost | cheap | **expensive** — 1 rollout = 1 full sandboxed session (minutes, real spend) |

**Convergence (the elegant part):** a distilled ACE bullet *is* a seed candidate
for GEPA; a GEPA-evolved instruction *is* a high-confidence playbook bullet. One
on-disk schema, one event-log-derived admission gate that does double duty
(regression guard **and** anti-poisoning filter).

**pillbox's unique enabler — closed-loop attribution:** the event log records
both each session's outcome *and which playbook/profile version was injected*,
so you can compute each artifact's empirical win-rate and **decay** ones that
stop correlating with success — the curation competency most memory systems
fail.

## The unified artifact schema (one type for both loops)

```jsonc
{
  "id": "...", "scope": "user|project|task-class", "kind": "episodic|procedural|semantic",
  "title": "...", "applicability_conditions": "...",
  "content": "the playbook / instruction text",        // abstracted, code-free
  "provenance": "SIGNED pointer to session id + event-log offsets",
  "helpful_count": 0, "harmful_count": 0, "reward_delta": 0.0,
  "embedding": [ ... ], "created_at": "...", "superseded_by": null
}
```

This is simultaneously an aithy episode, a Claude Skill, an ACE bullet, and a
GEPA candidate.

## Serving over MCP — the substrate principle, encoded in the protocol

Keep the tool surface **tiny (2–4)** — a fat MCP server taxes every attached
agent multiplicatively. The tools-vs-resources fork *is* "substrate exposes
primitives, consumer decides," expressed as "who decides when to use it":

- **TOOLS** (model-controlled): `search_memory` (returns a *small reranked
  top-k*; filter/dedup/rank **in code**, return only the hits) and
  `write_episode` (targets a **staging lane** the distiller curates — never the
  served set directly).
- **RESOURCE** (host-controlled): `get_playbook` / `active_policy` for a *known*
  task class — pre-injected via the existing `--mcp` tempfile/resource path.
  **Zero live server, zero per-run latency, zero tool-def tax**, deterministic
  "which playbook applies" routing in code. This is the compile-time-output
  path; prefer it over live `search_memory` for known task classes.

(Caveat: Anthropic's Tool Search / Programmatic Tool Calling that would mitigate
the context tax are agent-side and not enforceable across codex/opencode — don't
design around them.)

## Distillation, scoping, curation (all external)

- **Distillation (write side, offline — never inline):** an out-of-band
  distiller (Letta "sleep-time" / Claude "dreaming"; on the `session prune`
  cadence) reads *successful* completed sessions, gates on success (Memp), and
  emits bullets. ACE Generator/Reflector do language+judgment; the Curator's
  delta-merge/dedup/counter logic is deterministic **code** (RLM-clean).
  Reflection-update: when an injected bullet preceded a failure, **revise/demote
  it — don't append a contradiction.**
- **Scoping:** default **per-project private**. Reads = scoped top-k. Writes to
  the *served* pool only from the distiller, after the admission gate; live
  agents write only to the staging lane.
- **Curation / forgetting (first-class):** bound the store, dedup at write time,
  **supersede-not-delete** (stale procedures stop being injected but stay
  auditable), decay bullets whose win-rate goes negative. **No graph DB** until
  flat-vector + tags + supersession demonstrably breaks (Zep/Graphiti cut
  against the no-daemon ethos).

## Privacy — the real moat (not the optimizer)

A 5-stage gate on anything entering a *shared* pool (RLM-clean: parse/filter in
code, model only for the generalize step):

1. **Distill** event log → candidate bullets.
2. **Generalize** (the load-bearing privacy step): rewrite episode → API-level /
   approach-level rule (Voyager "skill not action"; Memp "rule not transcript").
   Abstraction strips literal code *by construction*; **never admit verbatim
   trajectories or raw code** into a cross-user pool (MEXTRA extracts raw demos
   from pools via crafted queries — which is also why every demo-based DSPy
   optimizer, BootstrapFewShot/KNN/Labeled, is OUT for pillbox).
3. **Strip identifiers** deterministically: exact-match against pillbox's **own
   vaulted secrets first** (the MITM knows the real values), then
   Gitleaks/TruffleHog entropy+regex, then a semantic PII pass.
4. **Validate**: admit only if it measurably lifts held-out pass@1 / cuts
   failures. **This — not provenance — is the real anti-poisoning defense**: a
   legitimately-signed session can still distill a subtly-bad procedure ("skip
   the flaky test to get green").
5. **Admit**: embedding-indexed, provenance + counters, demote when counters go
   negative. A **human approve/reject/modify gate** before a bullet becomes a
   cross-user *default* ("patterns are inferences, not facts").

**Honest about the guarantee:** "exact-match scrub against vaulted secrets =
zero false negatives" holds **only for known-provider hosts under strict-deny
egress**. The vault passes non-matched hosts through unmodified
(`vault/server.rs:6`), so a secret exfiltrated elsewhere never transits an
inspected path — **strict-deny egress (403 on unmatched) must land before any
cross-user pooling.** Pool **signal by default, content only by opt-in** (FedPOB:
sharing only the helpful/harmful *counters* improves everyone, and gains grow
with participants — the counters are shareable even when bullet text isn't).
Managed-tier (SAMEP shape): per-user private verbatim memory *below* an opt-in
shared pool of scrubbed+validated abstracted bullets, retrieved by embedding.

## The five substrate primitives pillbox must ship (none exist today)

The loops are external; pillbox's only obligation is the contract. Verified gaps:

1. **Verifiable, non-self-reported reward channel** — external grader on the
   rustic result-snapshot + exit code. `session.completed` is self-stamped
   (`session.rs`), Goodhart-banned. *Gates the whole compile-time loop.*
2. **Persisted traces** — Harden #2 `raw_body` blob store; bodies incl. reasoning
   are *dropped* today (`genai_tap.rs:193`), so "the log is an ASI source" is
   aspirational until it lands.
3. **`content/signal` `class` field** on every event/blob (specced in
   [session-event-log.md](./session-event-log.md), absent from src) →
   safe-by-schema pooling.
4. **Per-actor / per-session scoped MCP tokens** — today `--mcp-token` is one
   bearer per attachment (`agents/mcp.rs`); cross-user pooling needs per-actor
   write attribution + read scoping.
5. **Strict-deny vault egress** — see the privacy guarantee above.

## Open questions

- Does GEPA's sample efficiency survive **long-horizon coding**? The 35× / +20%
  numbers are short-horizon, densely-graded benchmarks. Cost one pillbox rollout
  before committing.
- Minimum viable **verifiable-reward harness** on a user's *own* tasks (usually
  no packaged test harness) — is it "the user's CI/test exit code," and is that
  gameable?
- Does the **GEPA Pareto output and the ACE bullet pool** genuinely share one
  on-disk schema in practice, or does the granularity mismatch force two?
- Cold-start poisoning: the held-out gate has latency (a bad bullet can serve
  before enough sessions accrue to demote it). What's the small-early-pool
  safety story? (MINJA-style injection is >95% on small pools — keep top-k tiny,
  default private.)
- Where does the human review gate sit without throttling the circulation
  pillbox is short on?
