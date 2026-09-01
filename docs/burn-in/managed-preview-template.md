# Managed preview burn-in runbook

This is an operator template for the isolated Pillbox managed-preview gate.
It is intentionally a small, manual release exercise for the Huddles execution
consumer. It does not deploy production, turn on public tool-enabled runs, or
create a Durable Object owned by Pillbox.

## Scope and reviewed budget

The version 3 report contract in
`cloudflare-spike/testdata/burnin-reconciliation.fixture.json` records one
genuinely new managed execution. The release harness performs these steps in
this order, with Huddles as the only live runtime recorder:

1. reject a managed Codex request during local preflight, before provisioning;
2. let Huddles execute one deny-all OpenCode turn over its private service binding;
3. let Huddles retry the exact request;
4. let Huddles read status twice with `evidence_after` 0 and 100, each limited to
   100 events, then write the authoritative report-v3 partial;
5. let Pillbox validate that terminal partial and finalize the same session over
   scoped public HTTP, attaching the result snapshot to the same report.

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

## Bootstrap boundary

OPS-000 owns creation and validation of the isolated D1/R2 resources, database
migrations and allowance row, signing/currentness keys and pins, capability
secret, and installation/workspace/policy identity. This runbook consumes the
checked OPS-000 outputs; it does not provide an alternate manual bootstrap.
Do not enable the Worker unless the applied bootstrap receipt proves the complete
matching tuple and the one-run allowance.

The committed `wrangler.burnin.toml` is deliberately non-deployable: its public
pin values are empty and its parse-only D1 UUID is rejected by the bootstrap
validator. Materialize a protected local copy from the HD-013/OPS-000 output;
never replace the template with guessed identifiers. The non-secret Huddles
tuple has schema `huddles.pillbox-burnin-bootstrap/1`, must come from `--apply`, and is authoritative for
the signer key ID/public key/fingerprint, installation, execution realm,
protocol revision, organization, currentness service/entrypoint, and concurrency
of one. A dry-run tuple is never deployment evidence. The applied tuple also
contains an `huddles.pillbox-burnin-authority-receipt/1` receipt with an exact
application time and canonical receipt digest. Its database tuple, issuer
key-pair self-test, and currentness probe must all be `verified` for the same
installation, policy, key ID/fingerprint, and service entrypoints. Wrangler
configuration and secret-name metadata alone do not prove applied authority.

Install the mandatory public-HTTP capability secret through Wrangler stdin.
The secret must not appear in a command argument, shell trace, config, metadata,
or terminal output:

```sh
set +x
umask 077
read -r -s PB_BURNIN_CAPABILITY_SECRET
printf '%s' "$PB_BURNIN_CAPABILITY_SECRET" |
  npx wrangler secret put MANAGED_CAPABILITY_SECRET \
    -c /private/path/wrangler.burnin.resolved.toml
npx wrangler secret list \
  -c /private/path/wrangler.burnin.resolved.toml \
  --json \
  > /private/path/wrangler-secret-metadata.json
```

`wrangler secret list` records names and types only; check that the protected
metadata file contains `MANAGED_CAPABILITY_SECRET`, never its value. Validate
the complete resolved tuple before any dry-run, migration, seed, or enablement:

```sh
node cloudflare-spike/scripts/validate-burnin-bootstrap.mjs \
  --bootstrap /private/path/huddles-pillbox-burnin-bootstrap.json \
  --config /private/path/wrangler.burnin.resolved.toml \
  --metadata /private/path/wrangler-secret-metadata.json
```

The validator fails on dry-run mode, a missing or mismatched applied-authority
receipt, missing secret name, empty or placeholder resource, invalid
key/fingerprint, missing pin, tuple mismatch, wrong currentness binding, or
concurrency other than one. It requires exactly one isolated D1 database, R2
bucket, `RUN_COSTS` Analytics dataset, vendor `Sandbox` DO/container/migration
tuple, and currentness service. Its successful output contains public evidence
and the word `installed`; it never reads or prints secret material. The Worker
repeats the executable subset of this preflight before routing any public
request. Huddles continues to use its private service binding plus signed,
current operation grants; this public HMAC secret does not replace that path.

The config contains only the vendor-owned `Sandbox` Durable Object class and
sets `max_instances = 1`. Do not add `SessionGateway`, a VFS class, or any
Pillbox-authored DO class.

## Topology gate and dry run

Before enabling the Worker, run the repository gates. The dry run is the only
burn-in package script that invokes Wrangler deployment tooling; there is no
managed-preview or production deploy automation.

```sh
npm test
npx tsc --noEmit
npm run check:contract
node --test do_usage_policy.test.mjs
npx wrangler deploy \
  -c /private/path/wrangler.burnin.resolved.toml \
  --dry-run --containers-rollout=none
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
npx wrangler deploy -c /private/path/wrangler.burnin.resolved.toml \
  --var MANAGED_EXECUTION_ENABLED:1 \
  --containers-rollout=none
```

First run the real managed-Codex preflight from Pillbox. This phase performs no
execute, status, or finalize request:

```sh
cd /path/to/pillbox/cloudflare-spike
BURNIN_MANAGED_PREFLIGHT=/path/to/pillbox/scripts/smoke/managed-agent-preflight.sh \
npm run burnin -- --preflight \
  --record /private/path/pillbox-preflight.json
```

Next run Huddles with that attachment. Huddles performs the only created execute,
the exact retry, and both bounded status reads over its authenticated private
service-binding bridge. It writes the authoritative version 3 partial with
`capture.cleanup: null`:

```sh
cd /path/to/huddles
BURNIN_CONFIRM_ISOLATED=1 \
PILLBOX_BURNIN_OPERATOR_TOKEN=<huddles-operator-token> \
CLOUDFLARE_ACCOUNT_ID=<account-id> \
BURNIN_PILLBOX_PREFLIGHT_FILE=/private/path/pillbox-preflight.json \
node app/coordinator/scripts/pillbox-managed-burnin.mjs \
  --execute --record /private/path/burnin-report.json
```

Only after that command returns terminal positional evidence, run Pillbox
finalize against the same report and session. This is the separately scoped
public HTTP boundary; it has no execute or status capability:

```sh
cd /path/to/pillbox/cloudflare-spike
BURNIN_CONFIRM_ISOLATED=1 \
BURNIN_BASE_URL=https://pillbox-managed-burnin.<preview-domain> \
BURNIN_FINALIZE_TOKEN=<exact-finalize-capability> \
BURNIN_FINALIZE_REQUEST_FILE=/private/path/finalize.json \
npm run burnin -- --finalize \
  --report /private/path/burnin-report.json
```

Use the short-lived, exact-request capabilities produced from the checked
OPS-000 bootstrap. Do not put a provider key, workspace credential, or bearer
token in the manifest, report, or a committed file. If minting the scoped public
finalize capability locally, export the exact same local secret that was sent to
Wrangler; do not generate a second HMAC key:

```sh
PILLBOX_MANAGED_TOKEN_SECRET="$PB_BURNIN_CAPABILITY_SECRET" \
  pillbox run --managed
unset PB_BURNIN_CAPABILITY_SECRET
```

Mint only the short-lived finalize capability for its canonical request bytes.
Clear the local secret after the token exists; never write it or bearer tokens
into the bootstrap tuple, Wrangler metadata, manifest, report, or Git.

The exact retry reuses the same Huddles invocation and one allowance reservation.
The Codex step is a local preflight and must make zero HTTP requests and zero
Sandbox provisions. Pillbox rejects finalize unless the Huddles partial proves
one created execution, one reused retry, both status pages, immutable positional
evidence, one artifact, and one cost envelope for the same session. Finalize is
the sole cleanup request; there is no separate cancel call. A duplicate execute,
cleanup that predates terminal evidence, a second allowance, an unbounded page,
a second artifact, or a second Analytics point is a failed gate.

## Capture and reconcile provider counters

Huddles writes one strict report shape and Pillbox attaches only cleanup:

- `capture.execution_identity` is recomputed from the canonical first execute
  request and seals invocation/idempotency/session identity, request hash,
  execution digest, and policy revision.
- `capture.runtime_calls` contains exactly the execute, exact retry, and two
  bounded status calls. Each record carries the full returned identity,
  positional session range and evidence cursor, immutable artifact reference
  (including bytes and digest), and cost reference.
- `capture.preflight` contains the observed local rejection and zero side-effect
  counters. It has no execution artifact or cost fields.
- Huddles initially writes `capture.cleanup: null`. Pillbox replaces only that
  field after validating the terminal partial and completing public finalize for
  the same session. Cleanup has no execution artifact or cost fields.

Copy the report to a private working location, attach the per-run `observed`
counters, and fill its `capture.read_only` and `capture.totals` objects from the
same isolated run. Capture all of these dimensions, including zeroes:

- D1 rows read and written;
- R2 reads, writes, and bytes;
- Container starts, duration/profile, and cleanup calls;
- Worker requests;
- Analytics Engine points;
- vendor Sandbox/DO lifecycle/storage counters, including custom class list,
  starting/ending bytes, and custom storage delta.

The report's `run_cost_envelopes` must contain exactly one envelope for the one
new claim, bound again to the full execution identity and artifact digest. Its
D1/R2/Analytics/container counters must match the per-run
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
