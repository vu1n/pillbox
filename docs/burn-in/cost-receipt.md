# Managed cost reconciliation receipt

Approved direction: 2026-09-15. This operator-side receipt supplements, never
rewrites, the immutable execution artifact and `RunCostEnvelope` v1. It adds no
runtime persistence or provider calls. Huddles remains the sole runtime recorder.

Full reconciliation requires a separate `--receipt <path>` JSON document with
`schema_version: 1`, the report's exact `execution_identity`, `artifact_ref`, and
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
