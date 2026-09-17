# Managed cost reconciliation receipt

Approved direction: 2026-09-15. This operator-side receipt supplements, never
rewrites, the immutable execution artifact and `RunCostEnvelope` v1. It adds no
runtime persistence or provider calls. Huddles remains the sole runtime recorder.

Full reconciliation requires a separate `--receipt <path>` JSON document with
`schema_version: 2`, the report's exact `execution_identity`, `artifact_ref`, and
`cost_ref` (the envelope digest). Partial Huddles validation needs no receipt.
Missing, malformed, foreign, or incomplete receipts fail full reconciliation.

The receipt contains these independently sourced observations:

- `d1.pre_seal`: `{rows_read, rows_written}` measured before artifact sealing.
- `d1.terminal_commit`: the same pair measured for the terminal conditional update.
- `d1.execution_total`: independently measured whole-execute pair, equal to the
  sum of those two scopes. The report's per-run observed D1 pair must equal it.
- `container`: `instance_id`, `profile`, `execution_duration_ms`,
  `cpu_seconds`, `memory_byte_seconds`, `disk_byte_seconds`, and `egress_bytes`.
  Execution duration/profile match the envelope. Allocation and CPU units are
  lifecycle observations, not replacements for execution wall duration.
- `sources`: exactly `d1_pre_seal`, `d1_terminal_commit`, `d1_execution_total`,
  and `container_lifecycle`. Each records a nonempty provider `dataset`,
  `resource_id`, SHA-256 `capture_sha256`, and canonical ISO UTC `from`/`to`
  window with `from < to`. D1 sources name one database; the lifecycle source
  names the exact container instance. Source captures are retained separately;
  the receipt is operator evidence, not a cryptographic provider attestation.

Every unit is required and nonnegative/finite; rows and bytes are safe integers,
resource-seconds may be fractional. Null means unavailable and blocks the full
gate. Source windows for pre-seal and commit fit within the execution-total
window, and the lifecycle window contains that execution window.
Pre-seal ends no later than commit starts; the two accounting scopes cannot overlap.

The v1 envelope seals before terminal D1 persistence and includes one planned
terminal write. Consequently `pre_seal.rows_read == envelope.d1_rows_read` and
`pre_seal.rows_written + 1 == envelope.d1_rows_written`. Commit reads are measured,
never a hard-coded correction. Commit writes must equal the one planned write;
unexplained writes still fail. Existing R2 artifact, Analytics max-one, topology,
retry identity, and aggregate checks remain mandatory. Non-execution traffic
stays separately accounted in the report; no receipt field is a catch-all delta.

Container lifecycle usage is recorded independently of the report's execution
duration. No total dollar estimate is inferred without a versioned rate card;
no usage observation is called an invoice charge. Vendor DO, private issuer,
snapshot traffic, and retention still require their existing independent
accounting. A valid receipt alone does not close missing provider evidence.

```sh
node cloudflare-spike/scripts/reconcile-burnin.mjs /private/report.json \
  --receipt /private/cost-receipt.json
```

Checked-in receipts are synthetic tests, never live billing evidence.

## Explicit accounting scopes (v2)

Version 2 replaces v1 for full reconciliation; there is no legacy fallback.
Report-v3 and RunCostEnvelope-v1 remain unchanged. Their `totals.r2` covers
artifact traffic only, and `totals.worker` covers the runtime Worker only.
The receipt's required `accounting` object records the broader footprint without
silently widening either limit. All objects reject unknown fields.

Each group has exactly the named parts below and an independently observed
`total`. Every part and total is `{units, source}`. Units must be finite and
nonnegative; counters/bytes are safe integers. Only `duration_gb_seconds` may
be fractional. Group totals equal the sum of their parts. Missing measurements,
including missing zero-usage evidence, fail closed.

| Group | Parts (besides `total`) | Unit keys |
| --- | --- | --- |
| `d1` | `execution`, `retry_status`, `finalize`, `operator_inspection` | `rows_read`, `rows_written` |
| `r2` | `artifact`, `snapshot_restore`, `snapshot_finalize`, `verification`, `bucket_probes` | `reads`, `writes`, `lists`, `heads`, `deletes`, `bytes_read`, `bytes_written` |
| `workers` | `runtime`, `issuer`, `currentness`, `operator` | `requests`, `cpu_time_us` |
| `vendor_do` | `execute`, `cleanup_idle` | `sql_rows_read`, `sql_rows_written`, `duration_gb_seconds` |

Sources retain the existing dataset/resource/hash/UTC-window fields and add
`record_ids`: 1–512 unique nonempty identifiers locating provider observations
within the retained capture. Sibling partitions use one dataset and cannot
reuse a record for the same resource, even through a differently hashed capture.
Their windows must fit the total's window. A total is a separately identified
aggregate, not a relabelled partition record. These are auditable operator
assertions, not cryptographic provider attestations; the validator does not fetch
or authenticate captures. Empty analytics responses never prove zero.

D1 parts and total name the execution database. `execution` equals the existing
D1 receipt total and uses its exact execution window;
`retry_status + finalize` equals report `read_only.d1`. `retry_status` also
includes the fixed workload's required before/after allowance reads.
Inspection is additional overhead, not a catch-all adjustment. Retry/status
writes must be zero.

R2 parts and total name one bucket. `accounting.snapshot_prefix` is a nonempty
slash-terminated prefix disjoint from the artifact's entire `executions/`
namespace. Snapshot restore/finalize and verification source `key_prefix` equals
that prefix; artifact source `key_prefix` equals the exact artifact key;
bucket probes and total use `key_prefix: ""`. These selectors must be supported
by retained captures, not inferred from bucket-wide timing/size coincidence.
Artifact units equal report artifact totals, with zero list/head/delete units.
Restore and verification cannot write/delete; bucket probes can only list/head.
Extra snapshot writes therefore cannot explain an extra execution artifact.

Worker sources name four distinct scripts. `runtime` must name the report's
runtime Worker and match its request total. The total source names the account;
its units cover exactly those four scripts, not all account traffic.

Vendor DO sources name the receipt's exact container/DO instance. SQL row parts
match the report's per-run and non-execution vendor counters respectively;
duration is independently measured GB-seconds, never summed invocation wall
time or substituted execution milliseconds. `accounting.vendor_retention` is
`{before, after}`, each `{units: {stored_bytes}, source}` naming the same namespace,
with the before observation ending no later than the vendor lifecycle starts
and the after observation starting no earlier than that lifecycle ends.
It matches report namespace storage totals, is not added across runs, and does
not claim the namespace delta belongs exclusively to this invocation.

Container allocation remains measured once by the existing lifecycle receipt.
The new groups do not duplicate that allocation. Usage-scope reconciliation is
not complete dollar attribution: subscription/plan charges, database service
cost, retention byte-time, and between-run variance remain separate evidence
requirements. No all-in price or release approval is implied by a synthetic pass.
