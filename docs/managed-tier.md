# Managed tier — bounded execution on Cloudflare

Status: **experimental** (updated 2026-08-31).

Managed Pillbox is a single-controller execution runtime. It restores one
workspace into one Cloudflare Sandbox, runs one bounded agent turn, stores one
terminal claim and one immutable evidence object, then returns the evidence to
the caller's local session log. It is not a multiplayer session service.

<!-- brief:anchor managed-tier-runtime-boundary -->
Context: `doc://pillbox/managed-tier-do-gateway@0002#managed-tier-do-gateway`

## Ownership boundary

| Concern | Owner |
|---|---|
| Collaboration, participants, ordering, retries, cancel intent, fan-out | Huddles |
| Runtime policy, Sandbox placement, agent execution, output, cancellation, snapshot/evidence references | Pillbox |
| Container lifecycle and isolation | Cloudflare Sandbox SDK and its vendor-owned Durable Object |
| Bounded invocation/idempotency claim | D1 |
| Immutable terminal result and bounded evidence | R2 |
| One compact terminal usage point | Analytics Engine |
| Per-session §0 log for Pillbox CLI reads | Local `SessionLog` |

Pillbox has no custom Durable Object class, remote event sequencer, actor roster,
driver lease, WebSocket replay broker, or collaborative session database. The
historical `SessionGateway` implementation and Agents SDK dependency were
removed. Existing deployed class data is not deleted by this code change; any
namespace retirement needs a separate retention/export review.

## Execution lifecycle

1. The client snapshots its workspace to its rustic-on-R2 repository and sends
   scoped, short-lived transfer credentials with an exact provision capability to
   `POST /v2/workspaces/provision`.
2. `POST /v2/executions` validates and hashes the sealed request.
3. D1 claims the invocation with a point query/write. Exact retries reuse the
   row; changed content conflicts; an expired owner is interrupted, never
   re-sampled.
4. Cloudflare Sandbox runs the OpenCode turn within a five-minute and
   2,000-event bound.
5. Pillbox writes one immutable R2 artifact, terminalizes the D1 row, and emits
   at most one best-effort Analytics Engine point.
6. The client appends returned evidence plus the terminal cost envelope to its
   ordinary local session log. Finalize first stops every prompt-controlled
   process, then introduces a fresh transfer credential to the helper and records
   the canonical result snapshot.

The happy-path persistence budget is two D1 writes, one R2 object write, and one
Analytics Engine point. Status reads are bounded pages of at most 100 events.
There are no per-token, per-delta, PTY-frame, progress, or replay writes.

## Cost evidence

Every terminal run carries a versioned `RunCostEnvelope` containing raw units:
model tokens, provider-reported model cost, D1 rows read/written, R2 operations
and bytes, Analytics Engine points, Sandbox duration, and Sandbox profile.
Unknown dollar amounts remain unknown. `estimated_total_cost_usd` is absent
until a versioned infrastructure rate card exists. Local and managed runs are
inspectable with:

```sh
pillbox session cost <session-id>
pillbox session cost <session-id> --json
```

The immutable R2 artifact is the source of truth. Analytics is intentionally
best-effort and contains no prompt, output, secret, repository path, or
participant identity.

## Current limits

- Foreground execution only; managed detach/reconnect is unsupported.
- OpenCode over HTTP is the current executable capability.
- Public HTTP uses short-lived controller capabilities bound to one operation and
  exact session/invocation id. Huddles uses the trusted same-account service
  binding and owns participant/driver authorization.
- Public managed execution is `tool_policy: deny_all`. Tool-enabled execution is
  disabled until provider and workspace credentials have a brokered boundary;
  local microVM tools are unaffected.
- Request bodies are capped at 1 MiB, evidence at 2,000 events / 8 MiB, and
  responses and cursors are bounded and identity-checked by the CLI.
- The legacy Huddles `ensureSession`/`invokeSession` RPC methods are stateless
  compatibility adapters over the generic execution service.

## Cloudflare Computer

Cloudflare Computer is not part of this cutover. It is preview software and its
authoritative virtual filesystem is backed by Durable Object SQLite, so adopting
it could reintroduce the exact storage-cost risk this boundary removes. Evaluate
it later in an isolated preview namespace using the rubric in
[durable-object-usage.md](./durable-object-usage.md). No benchmark may silently
become a production dependency.

## Release gate

Before a managed deployment:

- run the topology policy test and the full TypeScript/Rust suites;
- verify the Wrangler binding list contains no Pillbox-authored DO class;
- reconcile application counters against Cloudflare D1, R2, Container, Worker,
  Analytics Engine, and Durable Object dashboards;
- configure account budget alerts at the low, medium, and emergency thresholds
  selected for that environment;
- verify a documented kill switch can stop managed execution without affecting
  local Pillbox.

The detailed default-deny rules are canonical in
[durable-object-usage.md](./durable-object-usage.md).
