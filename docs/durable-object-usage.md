# Durable Object usage policy

Status: **active** (2026-08-31). This policy implements
`doc://pillbox/managed-tier-do-gateway@0002#managed-tier-do-gateway`.

## Default rule

Durable Objects are default-deny for Pillbox product state. The only allowed DO
in the current managed topology is the Cloudflare Sandbox SDK's vendor-owned
`Sandbox` class, used strictly for container lifecycle and isolation.

A new Pillbox-authored DO class requires all of the following before code:

1. a ratified Brief amendment naming why D1, R2, Queues, Workflows, or local
   state cannot satisfy the requirement;
2. an operation budget for every request path (rows read, rows written, bytes,
   alarms, WebSocket messages, and retained state);
3. explicit cardinality and retention bounds with no startup/full-history scan;
4. per-run telemetry plus reconciliation against provider billing metrics;
5. low, medium, and emergency account budget alerts;
6. a tested kill switch and rollback path;
7. an isolated preview namespace that cannot mutate production state.

## Storage routing

| Data | Store | Constraint |
|---|---|---|
| Session owner, invocation reservation and claim, idempotency hash, lease, terminal references | D1 | bounded rows; primary-key point queries; opaque owner digest; no raw deltas |
| Raw/bulky evidence, logs, terminal output, snapshots | R2 | immutable/content-addressed objects; bounded object size |
| Aggregate run analytics | Analytics Engine | at most one compact point per terminal run; no content or identity |
| Pillbox CLI session log | local `SessionLog` | local single-controller sequencing |
| Container lifecycle | Cloudflare Sandbox DO | vendor-owned substrate only; no application log/state tables |

Never store one row per token, text delta, PTY frame, progress update, replay
event, or log line in Durable Object storage or D1. Never use recurring alarms
for polling, unbounded list/history reads, initialization scans, or unbounded
retention. Provider dashboards are not a substitute for application counters,
and application counters are not a substitute for invoice reconciliation.

## Run-cost contract

Each terminal execution records exactly one cost envelope in its immutable R2
artifact, and every terminal client response carries that same envelope. The
client rejects missing, inconsistent, non-finite, or out-of-budget cost evidence.
It contains raw provider and infrastructure units; it does not claim an all-in
dollar total without a versioned rate card. Its `analytics_points_planned` field
records the bounded terminal-write budget, never confirmed provider delivery.
After the immutable R2 write and conditional terminal D1 update, only that D1
winner attempts one derivative Analytics point. Failure is logged but cannot
undo terminal execution; retries and status reads emit no additional points.

An external [cost receipt](burn-in/cost-receipt.md) binds post-seal commit and
lifecycle observations to the immutable artifact and execution identity. It
reconciles measured scopes against the envelope's planned terminal write,
rather than inventing a fixed terminal-read correction or mutating evidence.
The receipt does not waive missing vendor DO counters or other release gates.
Receipt v2 names vendor SQL row units separately from non-SQLite storage units,
records lifecycle GB-seconds independently of execution milliseconds, and keeps
namespace retention outside additive per-run totals. Source-backed snapshot and
authority scopes cannot explain away extra execution artifacts or Analytics points.

Release owners must compare these envelopes with Cloudflare's D1, R2,
Containers, Workers, Analytics Engine, and Durable Objects metrics/billing
views. Cloudflare account budget alerts are daily projected-spend safeguards,
not real-time per-product circuit breakers, so the application kill switch is
still mandatory.

## Managed preview cost controls

The fixed managed-preview burn-in is deliberately limited to one genuinely new
execution. An exact retry reuses the D1 claim and immutable R2 artifact; bounded
status pages are read-only; unsupported managed Codex is rejected before
Sandbox provisioning; finalize is a separate kill-before-transfer operation.
The first environment therefore seeds `MANAGED_EXECUTION_LIMIT = 1` and the
matching `MANAGED_EXECUTION_EPOCH` after migration 0002. The D1 allowance is the
hard application breaker, does not auto-reset, and is reseeded only as a new
operator-reviewed epoch. `MANAGED_EXECUTION_ENABLED = 0` remains the default
kill switch and does not affect local libkrun Pillbox.

A genuinely new invocation first inserts one durable reservation. The insert
atomically binds or checks the session owner and increments the allowance;
failure rolls back all three effects. Provision is the initial invocation's
reservation and execution consumes it without another increment. Exact retries
perform only bounded point lookups. A crash after provision reservation is
fail-closed: the row remains consumed and restore is never repeated from
ephemeral credential material.

For every captured `RunCostEnvelope`, compare the per-run units and profile with
the same invocation's D1, R2, Container, Worker, Analytics Engine, and vendor
Sandbox/DO counters. Provider-observed Analytics writes remain outside the
immutable envelope and record explicit `observed - planned` variance; `-1` is a
truthful best-effort miss, while a positive variance violates the max-one plan.
Reconcile retry/status/finalize read-only activity in a separate bounded counter
set; never explain a second artifact, Analytics point, or model turn as a retry.
Fail closed on an unexplained delta, a status page
larger than 100 events, more than one artifact/Analytics point per new run, a
custom DO class, or any custom DO storage growth. The executable fixture and
operator report template are in
[docs/burn-in/managed-preview-template.md](burn-in/managed-preview-template.md).

For each environment, record an absolute monthly cap and configure alerts at:

- **low — 50%:** investigate the top run profiles and reconcile counters;
- **medium — 75%:** stop nonessential preview/benchmark traffic;
- **emergency — 90%:** disable managed execution with the kill switch and keep
  local Pillbox available.

These Cloudflare budget alerts are informational and account-wide; they do not
cap usage and must not be treated as a real-time breaker. The D1 allowance and
the explicit admission switch are the enforceable controls.

The release owner records the account, cap, alert recipients, and kill-switch
command in the private deployment runbook; secrets and account identifiers do
not belong in this repository.

## Cloudflare Computer evaluation gate

Computer may be evaluated only after the bounded execution cutover is stable.
The experiment must use a separate preview namespace and representative tasks.
Record, per successful task:

- task correctness and artifact parity;
- cold-start and total wall time;
- Durable Object rows read and written;
- Durable Object stored bytes before and after cleanup;
- R2/D1/Container/Worker/Analytics units;
- provider-reported model spend and total attributable cost;
- idle and teardown behavior.

The evaluation fails if Computer requires unbounded VFS growth, per-delta
persistence, lifecycle scans, unclear cleanup, or if its cost cannot be
attributed per run. Preview status alone forbids production adoption. Passing
the benchmark only authorizes an architecture proposal; adoption still needs a
ratified amendment and explicit budgets.

References:

- [Cloudflare Computer README](https://github.com/cloudflare/computer/blob/main/packages/computer/README.md)
- [Cloudflare Computer lifecycle](https://github.com/cloudflare/computer/blob/main/docs/11_lifecycle.md)
- [Durable Objects pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)
- [Durable Objects metrics](https://developers.cloudflare.com/durable-objects/observability/metrics-and-analytics/)
- [Analytics Engine pricing](https://developers.cloudflare.com/analytics/analytics-engine/pricing/)
