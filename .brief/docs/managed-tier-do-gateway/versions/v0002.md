---
id: managed-tier-do-gateway
project: pillbox
type: decision
status: active
title: Huddles owns collaboration; Pillbox managed is a single-controller execution runtime
related_code:
  - "src/sandbox/managed.rs"
  - "src/sandbox/mod.rs"
  - "src/events/source.rs"
  - "cloudflare-spike/**"
---

<!-- brief:anchor managed-tier-do-gateway -->
## Huddles is the gateway; Pillbox executes one sealed invocation

The managed tier is a single-controller execution service. Pillbox validates and
enforces one sealed execution request, addresses an isolated Cloudflare Sandbox,
streams runtime-local output, performs cancellation, snapshots the workspace,
and returns terminal evidence plus artifact references.

Pillbox does not own the multiplayer boundary. Huddles owns participants, roles,
visibility, collaborative ordering, input arbitration, scheduling, retries,
cancellation intent, fan-out, reconnect, and durable WorkEvents. A Huddle may
coordinate several Pillbox executions, so a gateway scoped to one runtime cannot
be the authoritative collaboration sequencer.

Pillbox retains only transport-safe execution identity below that boundary. A
stable execution id and idempotency key prevent a lost RPC response from
sampling a second model turn, and terminal status is queryable by that identity.
This is not a participant roster, collaborative scheduler, driver lease, or
shared event sequence.

### Persistence placement

- The Cloudflare Sandbox SDK's container-owning Durable Object is allowed as a
  substrate requirement. Pillbox must not add a second custom Durable Object as
  product-level session authority or use DO SQLite as its log warehouse.
- Immutable raw transcripts, runtime evidence, snapshots, and large outputs live
  in R2 under deterministic, bounded object keys. Live token deltas are streamed;
  they are not durable database rows.
- A relational store holds small execution claims, immutable request hashes,
  lifecycle status, terminal result references, retention metadata, and other
  query-oriented projections. It does not store one row per runtime delta.
- Runtime-local positions may support diagnostics, artifact addressing, or an
  explicitly bounded stream-recovery window. They are not the Huddles timeline;
  Huddles chooses which semantic events become WorkEvents and assigns their
  canonical collaborative order.
- Shared memory remains outside Pillbox core. It asynchronously consumes
  completed execution artifacts and stores project/user-scoped claims separately.

### Durable Object cost boundary

Custom Durable Object storage is **denied by default**. A future custom DO must
be authorized by a new ratified decision that names the coordination invariant
requiring single-instance linearizability and explains why the existing
Sandbox DO plus relational metadata cannot satisfy it. That decision must also
provide a per-execution read/write budget, p50/p99 and worst-case row estimates,
bounded retention, indexed query shapes, a kill switch, and billing alerts.

Even under such an exception:

- Never persist token, text, thinking, PTY, or progress deltas as individual DO
  rows. Coalesce semantic output and place raw bodies in R2.
- Never use an unbounded replay, polling, or history query. Every online query
  must have an indexed predicate and an explicit row limit/cursor.
- Never scan a schedule, alarm, event, or invocation table at startup or on a
  recurring timer. Due-work queries must use an indexed due key and a hard batch
  bound.
- Never create recurring schedules from initialization unless creation is an
  atomic, uniquely keyed, idempotent singleton operation.
- Every retained table must have an explicit cardinality bound or deletion/TTL
  path. "The session is usually short" is not a bound.
- Load tests must assert storage-operation budgets before deployment, and the
  production surface must be independently disableable without deleting data.

### Invariant

- The managed backend provisions nothing on the host; managed execution is a
  network call into Cloudflare placement.
- Huddles owns collaboration semantics and the collaborative event journal.
  Pillbox owns sandbox execution and runtime artifacts only.
- A retry with the same execution identity and request hash never samples a
  second model turn; changed content under that identity fails closed.
- Workspace transfer uses a fresh, prefix-scoped R2 credential per transfer.
  The bucket-wide parent key never crosses to Cloudflare, and the scoped
  credential's `session_token` is forwarded as `X-Amz-Security-Token`.
- R2 objects, relational metadata, runtime output, and results never contain raw
  OAuth tokens or unredacted provider authentication responses.
- No custom Pillbox Durable Object or DO storage is introduced without the
  separately ratified cost-and-coordination exception described above.

> **Migration note (ratified 2026-08-30):** the prior revision made a custom
> per-session SessionGateway DO the sequencer, replay store, actor authority,
> driver arbiter, and Huddles invocation ledger. That ownership is retired.
> Huddles must land its gateway-side execution adapter before the historical
> service-binding surface is removed. Existing experimental SessionGateway data
> is preserved or exported under an explicit retention decision; it is never
> silently reinterpreted as Huddles WorkEvents.
