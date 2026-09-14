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
Incoming Worker, authorization/currentness, status, and D1 reads, operator-side
rustic snapshot traffic, and provider infrastructure remain separately
attributable costs.

Public HMAC and private Huddles authorization deliberately produce different
owner domains. A session provisioned by a public controller can only be executed
by that public controller owner; a private Huddles grant cannot adopt it. The
current Huddles burn-in uses direct private execution and trusted operator
finalize. A future Huddles workspace restore needs an authenticated,
owner-preserving broker contract rather than a cross-domain bypass.

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
observed Analytics writes are attached later during burn-in reconciliation with
an explicit `observed - planned` variance; they never rewrite the envelope.
Analytics contains no prompt, output, secret, repository path, or participant
identity.

The v1 envelope is sealed before terminal persistence: its D1 reads are pre-seal
measurements and its writes include one planned terminal write. Full burn-in
reconciliation requires an identity/artifact-bound [cost receipt](burn-in/cost-receipt.md)
with separately observed commit rows and container lifecycle allocation units.
It never rewrites the artifact or assumes execution duration equals billed
lifecycle duration. Missing receipt observations keep the gate closed.

## Current limits

- Foreground execution only; managed detach/reconnect is unsupported.
- OpenCode over HTTP is the current executable capability.
- Managed agent support is admitted at the first `ManagedBackend::run`
  boundary. Any other agent, including Codex, returns
  `unsupported_execution` before workspace resolution/snapshot, local session
  persistence, scoped credential minting, provisioning, execution allowance,
  or network access. This gate does not affect Codex on the local libkrun
  backend.
- Public HTTP uses short-lived controller capabilities bound to one operation,
  the exact bounded request bytes, and the exact session/invocation id. Huddles
  uses the trusted same-account service binding and owns participant/driver
  authorization.
- Every session is durably bound to one opaque owner domain and digest at its
  first invocation reservation. Huddles ownership derives from the verified
  installation, execution realm, organization, and workspace; public HMAC
  controllers occupy a separate domain. Principal, policy, grant, signer, and
  credential material are not persisted. Legacy rows without that provenance
  fail closed.
- Public managed execution is `tool_policy: deny_all`. Tool-enabled execution is
  disabled until provider and workspace credentials have a brokered boundary;
  local microVM tools are unaffected.
- Request bodies are capped at 1 MiB, OpenCode control responses and SSE frames
  are capped before parsing, evidence is capped at 2,000 events / 8 MiB, and
  responses, costs, and cursors are bounded and identity-checked by the CLI.
- Historical `ensureSession`/`invokeSession` adapters are local-test-only and
  accept no managed authorization. The production Huddles entrypoint exposes
  only authenticated v2 execute/status/cancel operations.

## Cloudflare Computer

Cloudflare Computer is not part of this cutover. It is preview software and its
authoritative virtual filesystem is backed by Durable Object SQLite, so adopting
it could reintroduce the exact storage-cost risk this boundary removes. Evaluate
it later in an isolated preview namespace using the rubric in
[durable-object-usage.md](./durable-object-usage.md). No benchmark may silently
become a production dependency.

## Managed preview release gate

Before any managed preview deployment, run the fixed burn-in described in
[docs/burn-in/managed-preview-template.md](burn-in/managed-preview-template.md)
against a namespace whose Worker, D1 database, R2 bucket, Analytics dataset,
and Sandbox container class are all distinct from the existing preview. The
first reviewed workload has exactly **one genuinely new execution**: one
deny-all OpenCode execute, one exact retry, two status pages capped at 100
events, a managed-Codex preflight rejection, and one kill-before-transfer
finalize. The retry/status/finalize calls do not increase the execution claim
allowance. Therefore the initial environment must set:

```toml
MANAGED_EXECUTION_ENABLED = "0"
MANAGED_EXECUTION_EPOCH = "burnin-2026-09-01-v1"
MANAGED_EXECUTION_LIMIT = "1"
```

The operator applies migration 0002 first, seeds the singleton
`managed_execution_allowance` row with the same epoch and limit, and only then
explicitly enables the isolated Worker. The application allowance is the hard
breaker and never auto-resets; changing the epoch/limit is a new reviewed run.
There is no production deploy automation.

The gate must:

- run the topology policy test and the full TypeScript/Rust suites;
- verify the Wrangler binding list contains no Pillbox-authored DO class;
- execute the deterministic workload, including an exact retry and bounded
  status pages;
- run `scripts/smoke/managed-agent-preflight.sh` through the workload recorder
  and retain its observed rejection output/hash and zero provision, network,
  and state-write counters; copied manifest expectations are not release
  evidence;
- reconcile every `RunCostEnvelope` against captured D1, R2, Container, Worker,
  Analytics Engine, and vendor Sandbox/DO counters, keeping planned Analytics
  units separate from provider-observed writes and recording their variance;
- fail on unexplained counter deltas, more than one immutable artifact or
  Analytics point per new run, any custom DO class, or custom DO storage growth;
- configure account budget alerts at 50%, 75%, and 90% of the small preview
  cap, and verify the documented kill switch stops managed execution without
  affecting local Pillbox.

The detailed default-deny rules are canonical in
[durable-object-usage.md](./durable-object-usage.md).
