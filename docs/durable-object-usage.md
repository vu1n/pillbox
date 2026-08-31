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
| Invocation claim, idempotency hash, lease, terminal references | D1 | bounded rows; primary-key point queries; no raw deltas |
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
dollar total without a versioned rate card. Analytics Engine receives at most
one derivative point, after the terminal D1 update. Retries and status reads emit
no additional points.

Release owners must compare these envelopes with Cloudflare's D1, R2,
Containers, Workers, Analytics Engine, and Durable Objects metrics/billing
views. Cloudflare account budget alerts are daily projected-spend safeguards,
not real-time per-product circuit breakers, so the application kill switch is
still mandatory.

For each environment, record an absolute monthly cap and configure alerts at:

- **low — 50%:** investigate the top run profiles and reconcile counters;
- **medium — 75%:** stop nonessential preview/benchmark traffic;
- **emergency — 90%:** disable managed execution with the kill switch and keep
  local Pillbox available.

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
