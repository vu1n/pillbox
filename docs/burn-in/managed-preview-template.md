# Managed preview burn-in runbook

This is an operator template for the isolated Pillbox managed-preview gate.
It is intentionally a small, manual release exercise for the Huddles execution
consumer. It does not deploy production, turn on public tool-enabled runs, or
create a Durable Object owned by Pillbox.

## Scope and reviewed budget

The fixed workload in
`cloudflare-spike/testdata/burnin-reconciliation.fixture.json` has one
genuinely new managed execution:

1. execute one deny-all OpenCode turn;
2. retry the exact request;
3. read two evidence pages, each limited to 100 events;
4. reject a managed Codex request during local preflight, before provisioning;
5. finalize the workspace after killing prompt-controlled processes.

The retry and status reads do not claim another execution, write another
artifact, or emit another Analytics Engine point. Therefore the first reviewed
allowance is exactly `1`, not the number of HTTP calls. The application
allowance is the hard breaker: it is reserved atomically in D1, does not
auto-reset, and must be reseeded deliberately under a new deployment epoch.

Managed execution is off by default in
`cloudflare-spike/wrangler.burnin.toml`:

```toml
MANAGED_EXECUTION_ENABLED = "0"
MANAGED_EXECUTION_EPOCH = "burnin-2026-09-01-v1"
MANAGED_EXECUTION_LIMIT = "1"
```

Do not change the limit to compensate for retries, status reads, or a failed
operator run. Stop, reconcile, and create a new reviewed epoch if the one
claim is consumed.

## Create isolated resources

Use a Cloudflare account/project reserved for preview. The names below must
remain distinct from the existing `pillbox-do-spike` resources.

```sh
cd cloudflare-spike
npx wrangler d1 create pillbox-managed-burnin-db
npx wrangler r2 bucket create pillbox-managed-burnin-evidence
```

Copy the returned D1 database id into `wrangler.burnin.toml`. Keep the D1 name,
R2 bucket name, Worker name (`pillbox-managed-burnin`), Sandbox container
class, and Analytics dataset (`pillbox_managed_burnin_costs`) isolated. The
Analytics Engine dataset is declared by the config; this Wrangler release has
no separate `analytics-engine create` command, so verify the declared dataset
in the account dashboard/API before the run. Configure an isolated Huddles
projector service for the `PillboxAuthorizationControlPlane` binding before a
private service-binding run.

The config contains only the vendor-owned `Sandbox` Durable Object class and
sets `max_instances = 1`. Do not add `SessionGateway`, a VFS class, or any
Pillbox-authored DO class.

## Apply and seed, in order

Run these commands manually and record their output in the private release
record. Migration 0002 must be applied before the operator allowance row is
inserted. The seed is deliberately separate from deployment so a code deploy
cannot silently reset the budget.

```sh
npx wrangler d1 migrations apply pillbox-managed-burnin-db \
  --remote --config wrangler.burnin.toml

npx wrangler d1 execute pillbox-managed-burnin-db \
  --remote --config wrangler.burnin.toml \
  --command "INSERT INTO managed_execution_allowance (singleton, deployment_epoch, execution_limit, reserved_executions) VALUES (1, 'burnin-2026-09-01-v1', 1, 0)"
```

The insert must be a singleton operator action. If the row already exists,
inspect its epoch and reservation count; do not use `OR REPLACE`, an update, or
a reset while investigating. The matching deployment epoch and limit are
required by every new claim.

## Topology gate and dry run

Before enabling the Worker, run the repository gates. The dry run is the only
burn-in package script that invokes Wrangler deployment tooling; there is no
managed-preview or production deploy automation.

```sh
npm test
npx tsc --noEmit
npm run check:contract
node --test do_usage_policy.test.mjs
npm run burnin:dry-run
npm run burnin
```

`npm run burnin` is deterministic and safe by default: it prints the exact
workload plan without making a network request. It must report one new claim,
one preflight rejection for managed Codex, bounded status pages, and a
kill-before-transfer finalize step.

## Execute the fixed workload

Enable only the isolated Worker, and only after the migration/seed and dry-run
gates pass. The default config remains off; enabling is an explicit operator
override, not a repository edit:

```sh
npx wrangler deploy -c wrangler.burnin.toml \
  --var MANAGED_EXECUTION_ENABLED:1 \
  --containers-rollout=none
```

Mint short-lived capabilities bound to the exact request bytes for each
operation. Do not put a provider key, workspace credential, or bearer token in
the manifest or in a committed file. Supply the isolated endpoint and tokens
through the shell environment, and supply finalize JSON from a protected local
file:

```sh
BURNIN_CONFIRM_ISOLATED=1 \
BURNIN_BASE_URL=https://pillbox-managed-burnin.<preview-domain> \
BURNIN_EXECUTE_TOKEN=<exact-execute-capability> \
BURNIN_STATUS_1_TOKEN=<exact-status-capability-1> \
BURNIN_STATUS_2_TOKEN=<exact-status-capability-2> \
BURNIN_FINALIZE_TOKEN=<exact-finalize-capability> \
BURNIN_FINALIZE_REQUEST_FILE=/private/path/finalize.json \
npm run burnin -- --execute --record /private/path/burnin-report.json
```

The exact-retry uses the same execute request and capability scope. The Codex
step is a local preflight and must make zero HTTP requests and zero Sandbox
provisions. A live response that attempts managed Codex, exceeds a status page
of 100 events, creates a second artifact, or returns a second Analytics point
is a failed gate; stop the Worker and preserve the evidence.

## Capture and reconcile provider counters

Copy the report to a private working location and fill its `capture.read_only`
and `capture.totals` objects from the same isolated run. Capture all of these
dimensions, including zeroes:

- D1 rows read and written;
- R2 reads, writes, and bytes;
- Container starts, duration/profile, and cleanup calls;
- Worker requests;
- Analytics Engine points;
- vendor Sandbox/DO lifecycle/storage counters, including custom class list,
  starting/ending bytes, and custom storage delta.

The report's `run_cost_envelopes` must contain exactly one envelope for the one
new claim. Its D1/R2/Analytics/container counters must match the per-run
captures. Read-only retry/status/finalize traffic belongs in `read_only`, not
in a second envelope. Reconcile with:

```sh
npm run burnin:reconcile -- /private/path/burnin-report.json
```

The reconciler fails closed on unexplained counter deltas, malformed/non-finite
cost units, more than one immutable artifact or Analytics point per new run,
unbounded status pages, any custom Durable Object class, or any custom DO
storage growth. A successful fixture check is:

```sh
npm run burnin:reconcile -- testdata/burnin-reconciliation.fixture.json
```

The checked-in fixture is deterministic test data, not a claim about an
account invoice. Replace it with the private report and retain the measured
per-run cost/variance alongside the Cloudflare dashboard exports.

## Budget alerts and stop conditions

Configure account-level projected-spend alerts at 50%, 75%, and 90% of the
small monthly preview cap. These Cloudflare alerts are informational and
account-wide; they do not cap usage and are not a real-time circuit breaker.
The D1 application allowance and `MANAGED_EXECUTION_ENABLED=0` kill switch are
the enforceable controls. At 90%, disable managed execution and leave local
Pillbox available. Never auto-reset the allowance or rely on an alert to stop a
run already admitted.

Do not proceed to Cloudflare Computer, public tool-enabled runs, managed Codex,
detach/reconnect, or production traffic until the burn-in report reconciles
against every provider dimension and the variance is understood.
