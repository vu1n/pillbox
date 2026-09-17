# Snapshot HTTP capture

This opt-in diagnostic records actual OpenDAL HTTP attempts below its retry,
pagination, and multipart layers. It is not provider billing evidence and never
rewrites a RunCostEnvelope or proves full cost reconciliation.

## Boundary

`rustic_backend` 0.6.1 is vendored with one constructor-only extension accepting
an OpenDAL HTTP client. Normal operations keep their existing backend. Captured
operations use explicitly supplied R2 credentials and a client with automatic
transport retries and redirects disabled. OpenDAL retries remain unchanged and
each attempt is recorded; redirects make capture incomplete rather than hiding
another request. No credential discovery or assume-role calls are permitted in
the captured path.

The observer stores no authorization headers, cookies, raw URLs, query strings,
request/response bodies, or error messages. Only allowlisted S3 actions and
decoded object keys/list prefixes within the configured bucket/prefix may be
recorded. Unknown operations, foreign selectors, transport failures, partial
bodies, dropped streams, or capture overflow leave the capture incomplete.

## Capture v1

The JSON object contains `schema_version: 1`, `capture_type: "r2_http_operation"`,
`capture_id` (fresh UUID), `operation` (`snapshot_restore`, `snapshot_finalize`,
or `verification`), `bucket`, normalized slash-terminated `prefix`, `started_at`,
`finished_at` (UTC), `status` (`complete` or `incomplete`), `reasons` (bounded
stable codes), and `records` (at most 256). Each record contains:

- `id`: capture ID plus a monotonically allocated sequence number;
- `method`, `action`, and `selector`: safe S3 action and exact decoded key or
  list prefix, at most 1,024 UTF-8 bytes;
- `status`: HTTP status, or null when no response was received;
- `request_body_bytes`: buffer bytes submitted to the HTTP client, not bytes
  known to have reached the server;
- `response_body_bytes`: observed bytes consumed through the response stream;
- `response_complete`: whether the response body completed without truncation,
  error, or premature drop.

Record IDs are `<capture_id>:<sequence>` with sequence starting at 1. Methods
are `GET`, `HEAD`, `PUT`, `POST`, `DELETE`, or `OTHER`. Actions are `GetObject`,
`HeadObject`, `HeadBucket`, `ListObjectsV2`, `PutObject`, `DeleteObject`,
`CreateMultipartUpload`, `UploadPart`, `CompleteMultipartUpload`,
`AbortMultipartUpload`, or `unknown`. An unrecognized or foreign request uses
`selector: null` and `action: "unknown"`; its raw target is never recorded.
`HeadBucket` alone uses the empty selector. Timestamps are UTC RFC3339 with up
to nine fractional digits. Counters must be JSON-safe nonnegative integers.
Reasons are a unique list of at most 10 codes: `record_limit`, `unknown_action`,
`foreign_selector`, `transport_error`, `redirect`, `response_error`,
`response_incomplete`, `operation_failed`, `counter_overflow`, and
`capture_unavailable`. A complete capture has no reasons, no unknown actions,
and only fully consumed response records with known HTTP statuses.

Each HTTP attempt has its own record, including retries and failures. A failed
connection is not proof of a billable request. Submitted upload bytes, consumed
response bytes, and logical snapshot bytes are distinct; none is relabelled as
invoice bytes. Multipart control requests remain distinct actions. Missing
telemetry is never reconstructed from a snapshot's logical size or elapsed time.

## Transport and ownership

`workspace restore --capture-r2-json` and `workspace backup --capture-r2-json` emit a bounded JSON
object on stdout: `{schema_version:1,status:"completed"|"failed",snapshot,capture}`.
`snapshot` is the backup result's canonical 64-hex handle (null for restore or
failure). Failure details remain outside capture; callers must not copy stderr
into it. Without the flag, ordinary command output is unchanged.

`snapshot show --capture-r2 PATH` writes the same capture to an exclusively
created 0600 sidecar while preserving normal snapshot JSON. The output is
reserved before network activity; failure must preserve incomplete evidence.
Local-disk repositories reject capture before storage work, not emit fake R2
zeroes. Cache policy is unchanged: cache hits produce no HTTP attempt.

The Worker opts in only through `MANAGED_R2_CAPTURE_ENABLED=1`. It validates the
helper output, binds it to the existing transfer response, and returns
`r2Capture`. It never persists this telemetry in D1 or a DO. Finalize still kills
prompt-controlled processes before introducing transfer credentials. Replays or
lost helper responses cannot reconstruct captures; they return explicit
unavailable telemetry. Transfer identity, ownership, and at-most-once claims
remain unchanged.

Helper status and the canonical result handle are validated independently of
the diagnostic capture. Invalid capture becomes unavailable telemetry; it does
not turn a validated successful transfer into a durable failure. Failed finalize
responses include the request digest and finalize ID so the operator can retain
valid incomplete capture without attaching a successful cleanup to its report.

The operator burn-in command accepts `--operation-capture-out PATH` with
`--finalize`. It reserves an exclusive 0600 sidecar before the request and
retains finalize and independent snapshot verification captures there, bound to
the execution identity, finalize ID, and request digest. It passes
`--capture-r2 PATH` to `pillbox snapshot show` in a private temporary directory;
validated telemetry is copied into the sidecar even when verification fails.
The finalizer must already have the opt-in Worker flag enabled; this command
does not deploy or enable it. Missing, replayed, or malformed captures remain
unavailable and do not cause another transfer attempt.

Report-v3 and receipt-v2 are unchanged. This enables future measurement, not a
new paid run, deployment, or release approval. HTTP-attempt observation remains
separate from provider-side billing totals and vendor lifecycle partitioning.
