# Operator vendor lifecycle capture

This document defines the bounded, operator-only capture produced by
`cloudflare-spike/scripts/capture-vendor-lifecycle.mjs`. It is evidence for a
later reconciliation review; it is not a runtime write, a billing export, or a
release attestation. The collector makes read-only requests to the fixed
Cloudflare GraphQL endpoint and must never be used to run, deploy, or mutate a
provider resource.

## Plan contract

The plan is JSON with exactly these fields:

```json
{
  "schema_version": 1,
  "account_tag": "32 lowercase hexadecimal characters",
  "namespace_id": "32 lowercase hexadecimal characters",
  "object_id": "64 lowercase hexadecimal characters",
  "application_id": "lowercase UUID",
  "instance_id": "64 lowercase hexadecimal characters",
  "windows": {
    "before": { "from": "2026-09-15T00:00:00.000Z", "to": "2026-09-15T00:10:00.000Z" },
    "lifecycle": { "from": "2026-09-15T00:10:00.000Z", "to": "2026-09-15T00:20:00.000Z" },
    "after": { "from": "2026-09-15T00:20:00.000Z", "to": "2026-09-15T00:30:00.000Z" }
  }
}
```

All fields are required. IDs are validated before any network request and are
serialized into GraphQL string literals only after that validation. Each window
is canonical ISO UTC (`Date#toISOString()` form), has `from < to`, and is at
most 24 hours. `before.to <= lifecycle.from <= lifecycle.to <= after.from` is
required. The collector intentionally has no execute/cleanup partitions: the
`lifecycle` window is one half-open `[from, to)` capture window, and the
namespace observations are separate before/after samples.

The CLI accepts `--plan <path-or-inline-json>` and `--out <path>`. The output
is opened with exclusive creation and mode `0600` before the first provider
request. `CLOUDFLARE_API_TOKEN` is the only token source. The token and request
headers are never written to the output or diagnostics.

## Capture contract

The output has this bounded shape; raw provider responses retain their original
fields. Example units below are synthetic, not live release evidence.

```json
{
  "schema_version": 1,
  "capture_type": "cloudflare_vendor_lifecycle",
  "status": "available",
  "captured_at": "2026-09-15T00:31:00.000Z",
  "plan": { "...": "canonical copy of the validated plan" },
  "bounds": {
    "max_window_hours": 24,
    "row_cap": 100,
    "max_requests": 4,
    "max_response_bytes": 1048576
  },
  "observations": {
    "do_lifecycle": {
      "status": "available",
      "row_count": 1,
      "units": {
        "sql_rows_read": 3,
        "sql_rows_written": 1,
        "duration_gb_seconds": 2.5
      }
    },
    "container_lifecycle": {
      "status": "available",
      "row_count": 1,
      "units": {
        "cpu_time_sec": 1.5,
        "allocated_memory": 100,
        "allocated_disk": 200,
        "tx_bytes": 300
      }
    },
    "retention_before": {
      "status": "available",
      "row_count": 1,
      "units": { "stored_bytes": 4096 }
    },
    "retention_after": {
      "status": "available",
      "row_count": 1,
      "units": { "stored_bytes": 8192 }
    }
  },
  "sources": {
    "do_lifecycle": {
      "status": "available",
      "dataset": "cloudflare_graphql",
      "resource_id": "account:<account_tag>/namespace:<namespace_id>/object:<object_id>",
      "window": { "from": "...", "to": "..." },
      "query": "raw GraphQL query",
      "http_status": 200,
      "response": { "raw": "decoded provider JSON" },
      "capture_sha256": "sha256:<64 lowercase hexadecimal characters>"
    }
  },
  "limitations": [
    "provider results are delayed and hourly/sample-granular",
    "lifecycle stop was not independently confirmed; this is a snapshot, not release proof",
    "R2 HTTP attempts require the separate opt-in operation capture",
    "no partition inference or billing total is made"
  ]
}
```

All four source entries are always present. Their `query` and bounded decoded
`response` are retained as the source record; `capture_sha256` covers the
canonical `{query,http_status,response}` record. Transport, malformed, and
oversized or more-than-32-level responses retain bounded metadata rather than unbounded error text or
body data. A source has `status: "unavailable"` and a short `reason` code when
its evidence cannot support a measurement. The top-level status is
`"available"` only when all four sources are available.

The four requests are:

1. `durableObjectsPeriodicGroups` for the exact account, namespace, and object;
   sum `rowsRead`, `rowsWritten`, and provider `duration`. The latter is
   Cloudflare Durable Object duration in GB-seconds. It is not invocation wall
   time, `activeTime`, or `cpuTime`.
2. `containersUsageAdaptiveGroups` for the exact application and instance;
   sum `cpuTimeSec`, `allocatedMemory`, `allocatedDisk`, and `txBytes`.
3. `durableObjectsSqlStorageGroups` for the namespace in the `before` window.
4. The same storage query in the `after` window.

Each query uses `limit: 100`; there is no pagination or retry. Empty, missing,
GraphQL-error, non-2xx, wrong-identifier, or `>= 100` row results are
`unavailable`, never zero. Every required numeric field must be present,
finite, and non-negative. SQL row counts and byte counters must also remain safe
integers after summation. Returned zero is retained as an observed zero, but
does not attest complete provider coverage. Storage observations report the
maximum `storedBytes` sample returned in their window; they do not claim that a
namespace delta belongs to one object or invocation.

For snapshot request observations, use the separate opt-in
[R2 HTTP capture](r2-http-capture.md). It does not substitute client counters for
provider billing evidence.

The provider may lag the workload and its hourly bins can begin before a
requested lower bound. The requested UTC window and raw bin labels are retained
without pretending that a bin is an exact operation timestamp. The capture does
not provide per-operation R2 attribution. It does not infer execute/cleanup partitions, receipt completeness,
invoice totals, or release approval. A live lifecycle that has not been
independently observed as stopped remains an open snapshot condition.
