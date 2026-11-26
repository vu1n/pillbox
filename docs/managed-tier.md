# Managed tier — bounded execution with selectable compute

Status: **experimental** (updated 2026-11-26).

Cloudflare remains the default workspace-capable substrate. The optional
[DigitalOcean adapter](../digitalocean/README.md) supports fresh sealed deny-all
OpenCode invocations using OpenRouter, with the same D1/R2 execution control plane.
The lifecycle below describes Cloudflare workspace execution; DO workspace and
native-session handoff are not enabled by provider selection.

Managed Pillbox is a single-controller execution runtime. It restores one
workspace into one Cloudflare Sandbox, runs one bounded agent turn, stores one
terminal claim and one immutable evidence object, then returns the evidence to
the caller's local session log. It is not a multiplayer session service. Every
caller is a public-HTTP controller holding a short-lived capability; there is
no private caller class.

## Ownership boundary

| Concern | Owner |
|---|---|
| Collaboration, participants, ordering, retries, cancel intent, fan-out | The calling controller (a collaboration service such as Huddles, outside Pillbox) |
| Runtime policy, Sandbox placement, agent execution, output, cancellation, snapshot/evidence references | Pillbox |
| Container lifecycle and isolation | Cloudflare Sandbox SDK and its vendor-owned Durable Object |
| Bounded invocation/idempotency claim | D1 |
| Immutable terminal result and bounded evidence | R2 |
| At most one compact derivative terminal usage point | Analytics Engine |
| Per-session §0 log for Pillbox CLI reads | Local `SessionLog` |

Pillbox has no custom Durable Object class, remote event sequencer, actor roster,
driver lease, WebSocket replay broker, or collaborative session database. The
historical `SessionGateway` implementation and Agents SDK dependency were
removed. Existing deployed class data is not deleted by this code change; any
namespace retirement needs a separate retention/export review.

## Execution lifecycle

1. The client prepares the initial invocation identity and request hash, snapshots
   its workspace to its rustic-on-R2 repository, and sends scoped, short-lived
   transfer credentials with that identity and an exact provision capability to
   `POST /v2/workspaces/provision`.
   Cloudflare's temporary-credential response may reuse the parent access-key
   ID; the client therefore requires a nonempty fresh secret access key and
   nonempty session token, and never accepts the bucket-wide parent secret.
2. Before Sandbox lookup or restore, D1 atomically binds the session to an opaque
   authorization owner and reserves one reviewed invocation allowance. Exact
   provision retries never restore twice; an ambiguous `provisioning` row stays
   consumed for operator reconciliation.
3. `POST /v2/executions` validates and hashes the sealed request. Its D1 claim
   consumes the matching ready provision reservation without reserving again.
   Direct and later invocations reserve once before runtime work. Exact retries
   reuse the row; changed content conflicts; an expired runtime owner is
   interrupted, never re-sampled.
4. Cloudflare Sandbox runs the OpenCode turn within a five-minute and
   2,000-event bound.
5. Pillbox writes one immutable R2 artifact, terminalizes the D1 row, and emits
   at most one best-effort Analytics Engine point.
6. The client appends returned evidence plus the terminal cost envelope to its
   ordinary local session log. Finalize first stops every prompt-controlled
   process, then introduces a fresh transfer credential to the helper. The helper
   records the provisioned base as the result snapshot's parent, and the client
   verifies that lineage in the authoritative rustic repository before persisting
   the result handle.

The execution-only happy-path persistence budget is one reservation claim, one
execution claim, one terminal D1 update, one R2 object write, and one Analytics
Engine point. An initial public provision uses its provision reservation in
place of the execution reservation, plus one readiness update; finalize retains
its separate claim and terminal update. Triggered session-owner and allowance
row mutations are counted explicitly during reconciliation. Status reads are
bounded pages of at most 100 events.
There are no per-token, per-delta, PTY-frame, progress, or replay writes.

The allowance bounds genuinely new managed invocations, not total spend.
Incoming Worker, capability verification, status, and D1 reads, operator-side
rustic snapshot traffic, and provider infrastructure remain separately
attributable costs.

Every session is owned by the `public_controller` domain, keyed by a digest of
the verified capability subject. A session provisioned under one capability
subject can only be executed by that subject. Migration 0004's CHECK constraint
still admits a retired second owner domain; nothing writes it any more.

## Cost evidence

Every terminal response must carry exactly one versioned, internally consistent
`RunCostEnvelope` containing raw units: model tokens, provider-reported model
cost, D1 rows read/written, R2 operations and bytes, planned Analytics Engine
points, Sandbox duration, and Sandbox profile. `analytics_points_planned` is the
bounded write budget, not evidence that Cloudflare accepted a point.
Unknown dollar amounts remain unknown. `estimated_total_cost_usd` is absent
until a versioned infrastructure rate card exists. Local and managed runs are
inspectable with:

```sh
pillbox session cost <session-id>
pillbox session cost <session-id> --json
```

The terminal commit order is immutable R2 artifact, conditional D1 terminal
update, then at most one best-effort Analytics emission by the D1 winner. A D1
terminal row prevents retries and recovery from emitting again. Analytics
failure is logged after commit and cannot undo terminal execution. Provider-
observed Analytics writes, if an operator reconciles them, are recorded outside
the envelope with an explicit `observed - planned` variance; they never rewrite
it.
Analytics contains no prompt, output, secret, repository path, or participant
identity.

The v1 envelope is sealed before terminal persistence: its D1 reads are pre-seal
measurements and its writes include one planned terminal write. It does not
include post-seal commit rows or container lifecycle allocation, and execution
duration is not billed lifecycle duration. No reconciliation tooling ships in
this repository; compare envelopes with provider billing views by hand.

## Current limits

- Foreground execution only; managed detach/reconnect is unsupported.
- OpenCode over HTTP is the current executable capability.
- Managed agent support is admitted at the first `ManagedBackend::run`
  boundary. Any other agent, including Codex, returns
  `unsupported_execution` before workspace resolution/snapshot, local session
  persistence, scoped credential minting, provisioning, execution allowance,
  or network access. This gate does not affect Codex on the local libkrun
  backend.
- Every route is public HTTP. Each request carries a short-lived HMAC
  capability (`MANAGED_CAPABILITY_SECRET`) bound to one operation, the exact
  bounded request bytes, and the exact session/invocation id. The Worker
  verifies it before any D1 or R2 access. Participant and driver authorization
  belong to the calling controller, not to Pillbox.
- Every session is durably bound to one opaque owner (`public_controller` plus a
  digest of the capability subject) at its first invocation reservation. Only the
  owner digest is persisted; capability, secret, and credential material are not.
  Legacy rows without owner provenance fail closed.
- Tool policy is request data (`tool_policy`) and must be `deny_all`. The public
  execute route rejects anything else with 400 `unsupported_policy` before D1
  access, and the DigitalOcean runtime rejects it too. Tool-enabled execution
  is disabled until provider and workspace credentials have a brokered boundary;
  local microVM tools are unaffected.
- Request bodies are capped at 1 MiB, OpenCode control responses and SSE frames
  are capped before parsing, evidence is capped at 2,000 events / 8 MiB, and
  responses, costs, and cursors are bounded and identity-checked by the CLI.
- The Worker exposes only the v2 execute/status/cancel and workspace
  provision/finalize routes. There is no private RPC entry point, service-binding
  caller, or operation-grant protocol; the historical
  `ensureSession`/`invokeSession` adapter was deleted on 2026-10-04.

## Cloudflare Computer

Cloudflare Computer is not part of this cutover. It is preview software and its
authoritative virtual filesystem is backed by Durable Object SQLite, so adopting
it could reintroduce the exact storage-cost risk this boundary removes. Evaluate
it later in an isolated preview namespace using the rubric in
[durable-object-usage.md](./durable-object-usage.md). No benchmark may silently
become a production dependency.

## Managed preview release gate

No burn-in harness exists now. The earlier manual burn-in, its workload
recorder, reconciler and cost-receipt scripts were removed along with the
private caller they served. A managed preview deployment needs a new
Pillbox-recorded burn-in first; that is future work and is not defined here.

What exists today is the application cost breaker. An isolated deployment must
use a Worker, D1 database, R2 bucket, Analytics dataset, and Sandbox container
class distinct from the existing preview, and start with:

```toml
MANAGED_EXECUTION_ENABLED = "0"
MANAGED_EXECUTION_EPOCH = "<reviewed epoch>"
MANAGED_EXECUTION_LIMIT = "<reviewed limit>"
```

The operator applies migration 0002, seeds the singleton
`managed_execution_allowance` row with the same epoch and limit, and only then
explicitly enables the Worker. The application allowance is the hard breaker and
never auto-resets; changing the epoch or limit is a new reviewed run. There is
no production deploy automation.

Checks that exist now:

- `npm test` in `cloudflare-spike/`, including `managed_policy.test.ts` and
  `managed_public_route.test.mjs` (the public route over wrangler
  `unstable_dev`), and `node --test do_usage_policy.test.mjs`, which fails on any
  Pillbox-authored DO class;
- `scripts/smoke/managed-agent-preflight.sh`, a standalone smoke of the Rust
  `ManagedBackend` admission boundary: Codex is rejected as unsupported with zero
  provision, network, and state-write counters;
- account budget alerts at 50%, 75%, and 90% of the preview cap, and the
  documented kill switch, which stops managed execution without affecting local
  Pillbox.

The detailed default-deny rules are canonical in
[durable-object-usage.md](./durable-object-usage.md).
