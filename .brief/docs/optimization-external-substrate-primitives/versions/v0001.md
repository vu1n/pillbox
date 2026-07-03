---
id: optimization-external-substrate-primitives
project: pillbox
type: decision
status: active
title: Optimization/swarm-memory lives outside pillbox; the substrate ships primitives, not the loop
related_code:
  - "src/contract.rs"
  - "src/commands/session/**"
  - "scripts/eval/**"
---

<!-- brief:anchor optimization-external-substrate-primitives -->
## pillbox is the substrate the optimization loop runs on — it does not host the loop

The optimization / collective-intelligence layer (DSPy, GEPA/`optimize_anything`,
ACE swarm memory, cost-routing) is **cut from this repo** and pursued externally
as a consumer of the pillbox contract. pillbox's only obligation is to ship the
substrate primitives the loop needs and keep the contract solid — never to embed
the optimizer.

**Why.** There is no single-feature moat and the optimizer itself is commoditized
(meta-harness is OSS; routing is LiteLLM/OpenRouter); the real moat is the
trace-rich, reproducible, secret-isolated substrate + the privacy of any
cross-user pooling. The verifiable reward must be **externally graded, never the
self-reported `session.completed`** (Goodhart). Embedding the loop would fuse
mechanism and policy and invite the optimizer to leak into the substrate.

### Invariant
- The reward channel is a non-self-reported verifier: `pillbox session score
  --cmd "<verifier>"` runs an external grader and appends `Payload::Scored`
  (grader exit/output is truth), never the agent's `session done --status`.
- Cross-user pooling shares **signal, not content** and runs only with default-deny
  egress on (see `vault-egress-default-deny`); the scrub abstracts to rules, never
  admits verbatim trajectories/code.
- The optimization loop is not compiled into pillbox; it consumes §0 over the
  `Subscribe`/`from_seq` contract.

> **Status (backfill 2026-07-01):** BUILT substrate primitive — the verifiable
> `Payload::Scored` reward channel (`session score --cmd`, `src/contract.rs`).
> **Not built** (specced in `docs/swarm-memory.md` / `docs/session-event-log.md`):
> persisted `raw_body` traces (dropped today), the content/`signal` `class`
> envelope field, per-actor scoped MCP tokens, and default-deny egress *on the
> host proxy path*. The eval rig (`scripts/eval/`) exists; the bakeoff verdict is
> parked (variance regime, not a missing harness — see
> `docs/optimization-eval-family.md`).
