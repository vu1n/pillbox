# Amendment proposal: managed-tier-do-gateway

**Decision:** doc://pillbox/managed-tier-do-gateway@latest#managed-tier-do-gateway

**Proposed by:** Codex `ship-it` run

**Date:** 2026-08-30

## What should change

Retire the requirement that Pillbox own a per-session multiplayer gateway and
co-located Durable Object SQLite event log in the managed tier. Replace it with
this boundary:

- **Huddles owns collaboration.** Its gateway is authoritative for participants,
  roles, visibility, collaborative event ordering, input arbitration, scheduling,
  retries, cancellation intent, fan-out, reconnect, and durable WorkEvents.
- **Pillbox owns execution.** It accepts one sealed execution from one controller,
  validates and enforces its runtime policy, addresses a Cloudflare Sandbox,
  streams runtime-local output, performs cancellation, snapshots the workspace,
  and returns terminal evidence and artifact references.
- Pillbox keeps only transport-safe execution identity and idempotency below the
  boundary. A stable `execution_id` / idempotency key must prevent a lost RPC
  response from sampling a second model turn, and callers must be able to query a
  terminal result. This is not a participant roster, scheduler, collaborative
  sequence, or driver lease.
- The Cloudflare Sandbox SDK's container-owning Durable Object may remain as a
  substrate requirement. Pillbox must not add a second custom Durable Object as
  the product-level session authority merely to host logs or multiplayer state.
- Immutable raw transcripts, runtime evidence, snapshots, and large outputs live
  in R2 as bounded objects. A relational store holds small execution metadata and
  result references. Live token deltas are streamed and need not become durable
  rows. Any runtime-local ordering exists only for stream recovery or diagnostics;
  it is not the Huddles timeline.
- Shared memory remains outside Pillbox core. It consumes completed execution
  artifacts asynchronously and stores project/user-scoped claims separately; it
  does not justify a per-runtime coordination gateway.

The existing managed security invariants remain in force: no host provisioning,
fresh prefix-scoped R2 credentials with the session token forwarded correctly,
and no raw OAuth tokens or unredacted provider authentication material in R2,
execution metadata, logs, or results.

## Why

A Pillbox is an isolated runtime with one controller. The multiplayer unit is a
Huddle, which may coordinate several Pillbox executions. A gateway scoped to one
Pillbox therefore cannot authoritatively order or arbitrate the collaboration; it
duplicates Huddles' workspace coordinator and places Huddles effect identities,
participant semantics, and replay state below the runtime boundary.

That misplaced ownership also made high-volume runtime deltas permanent rows in
Durable Object SQLite and encouraged replay through unbounded row scans. The
result is unnecessary read/write exposure for data that is naturally an
immutable execution artifact. Separating collaboration from execution gives
each system one authority and permits bounded R2 log objects plus low-volume
relational metadata.

The newer `pillbox.execution/2` design already establishes most of this seam:
Huddles owns `InvocationExecution`, policy selection, scheduling, retries,
cancellation intent, and orchestration identity, while Pillbox owns validation
and process supervision. The standing managed-tier decision predates and now
contradicts that separation.

## Code that needs it

After ratification, the implementation should:

- Replace `cloudflare-spike/src/session_gateway.ts` and the Huddles-specific
  `ensureSession` / `invokeSession` ledger with a generic, single-controller
  execution service built around the versioned execution contract.
- Remove Pillbox-owned roster, driver arbitration, actor-attested collaboration,
  public multiplayer attach, canonical collaborative sequencing, and permanent
  DO log storage from `cloudflare-spike/**`.
- Retain the Sandbox SDK binding and container lifecycle adapter, with execution
  status/idempotency metadata in the selected relational store and immutable
  transcript/result artifacts in R2.
- Refactor `src/sandbox/managed.rs`, `src/events/source.rs`, and related managed
  tests/docs so `SandboxBackend` / `LiveSession` do not imply that Pillbox owns a
  remote multiplayer gateway. Local runtime logs and attach behavior remain
  local substrate concerns.
- Change the Huddles integration to call the generic execution boundary from its
  own gateway. Huddles converts selected semantic runtime events into WorkEvents;
  raw harness deltas are not copied into the collaborative journal.

## Impact / risk

- **Cross-repository rollout:** Huddles must gain the gateway-side adapter before
  the historical Pillbox service-binding surface is removed. The versions need a
  staged compatibility window or an atomic deployment plan.
- **Lost-response safety:** moving orchestration upward must not permit duplicate
  model turns. The execution claim, terminal status query, and immutable request
  hash are required before deleting the current invocation ledger.
- **Reconnect semantics:** Huddles must persist enough collaborative state to
  recover its own clients. Pillbox only promises the explicitly versioned runtime
  stream/result recovery contract.
- **Event-model split:** Huddles owns canonical collaborative ordering; Pillbox
  may retain execution-local positions solely for diagnostics and artifact
  addressing. Consumers must not compare the two as one sequence space.
- **Existing experimental data:** current SessionGateway rows are not silently
  reinterpreted as Huddles WorkEvents. Preserve or export them according to an
  explicit retention decision before deleting the namespace.
- **Security:** the amendment narrows Pillbox's authority but does not weaken
  scoped R2 credentials, tool-policy enforcement, actor-independent runtime
  attribution, secret redaction, or default-deny behavior.


---
ratified_rev: 0002
ratified_by: maintainer
