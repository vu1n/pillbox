# Huddles managed Codex execution boundary

Status: sealed-envelope plus bounded ACP adapter spike. The existing managed
Huddles path remains OpenCode-only until the runtime and credential gates below
are implemented.

## Finding

The managed worker currently validates `harness: "opencode"`, stamps managed
agent events as `a:opencode`, drives the container through OpenCode on port
4096, and ships an image that installs only `opencode-ai`. Local
`codex-serve` is a separate libkrun-only app-server path. Reusing that bridge
in the Cloudflare container without changing its policy and credential
boundary would be unsafe: the current bridge auto-accepts Codex approval
requests, and managed Codex credential provisioning is not defined.

## Adapter evaluation

Pillbox should expose ACP as an explicit generic harness transport while keeping
native Codex app-server first-class:

```text
execution.transport.transport = "acp"         # portable ACP adapter
execution.transport.transport = "app_server"  # native Codex adapter
```

ACP is a process/event boundary, not an orchestration boundary. The bounded
spike borrows Buzz's substrate ideas—bounded NDJSON, correlated requests,
cancellation cleanup, crash interruption, and respawn for a later invocation—
but omits its relay, durable prompt queue, and agent-pool claim scheduling. A
second active turn returns `runtime_busy`; it is never queued. There is no
automatic ACP/app-server fallback.

For ACP, `session/new` receives only the policy-derived `mcpServers: []`, and
`prompt` receives exactly the sealed `rendered_input`. The injected event sink
gets `session_ref`, `invocation_id`, the computed execution digest, and policy
revision for attribution. No mutable ACP context can override the sealed HCP
packet, and the adapter emits neither HCP nor WorkEvent records. Huddles keeps
ownership of HCP/WorkEvent orchestration, retries, sequencing, cancellation
intent, and execution identity.

The spike checks one ACP result against the sealed `output_format` schema and
returns a safe structured-output failure without echoing provider content. It
does not retry inside ACP; any retry decision remains Huddles orchestration.

The Rust `sandbox::acp` module is deliberately a private supervisor seam only;
it has no host command or production dispatch yet. The Cloudflare adapter is
pure and injected-client based so its lifecycle contract is testable without
changing the current gateway, worker, OpenCode path, or deployment image.

## Versioned boundary

Pillbox adds a private `pillbox.execution/2` contract in
`cloudflare-spike/src/codex_execution.ts`. It is deliberately beside the
historical OpenCode RPC; the existing `ensureSession`/`invokeSession` request,
ledger rows, retries, and `a:opencode` evidence remain compatible and are not
reinterpreted.

The v2 request binds the substrate execution identity to the exact invocation
input and output contract:

```text
contract_version: "pillbox.execution/2"
session_ref: { session_id }
invocation_id
idempotency_key
rendered_input
rendered_input_hash: sha256:<64 lowercase hex>
tool_policy: "deny_all"
execution: Huddles InvocationExecution
execution_policy_revision
output_format: { type: "json_schema", schema, retry_count: 2 }
```

`execution` mirrors Huddles' authoritative broad contract: harness, transport,
version, adapter revision, requested provider/model/profile/reasoning effort,
optional placement, context-renderer revision, and optional verifier
reference. The boundary validator accepts that shape. Separate capability
checks refine it to native Codex over `app_server` or to any declared harness
over `acp`.

Pillbox recomputes the rendered-input hash over exact UTF-8 bytes. It also
computes an execution-identity digest over `{ execution,
execution_policy_revision }` and a whole-request hash for idempotency and
conflict detection. Neither digest is caller-supplied. Unknown policy or
execution capabilities remain fail-closed adapter decisions.

The boundary intentionally omits `workspace_id`, `effect_id`,
`delivery_receipt_id`, scheduling, retry, and claim/lock fields. Those are
Huddles orchestration identities or semantics. There is no generic mutex or
claim protocol.

The result carries terminal status, disposition, the computed execution digest
and policy revision, a positional `SessionRef`, and Codex attribution
(`a:codex` at the event layer). It may report `unsupported_policy`,
`auth_unavailable`, `runtime_busy`, interruption, cancellation, or structured
output failure without exposing provider diagnostics in the public result.

## Integration contract still needed in Huddles

Huddles remains responsible for constructing and authorizing
`InvocationExecution`, selecting and sealing the execution policy revision,
workspace and effect identities, scheduling, retries, cancellation intent, and
interpreting the result. Its future Pillbox adapter should call the v2 private
method with the rendered input, input hash, output format, and sealed
`execution_policy_revision`; it should not derive a second execution profile or
turn the policy revision into Pillbox orchestration.

Until that adapter exists, Huddles must continue using the historical ensure
RPC and provide its required top-level `requested_model` compatibility
projection. The v2 invocation envelope does not replace or migrate the
existing OpenCode ensure ledger.

Pillbox is responsible for validating the boundary, enforcing the known policy
before spawning Codex, launching the pinned app-server, normalizing
notifications into §0 events, stamping `a:codex` and invocation correlation,
sequencing/cancellation, and returning terminal evidence. This envelope carries
no credential field; any future credential capability must be specified and
scoped before the runtime adapter resolves one. Unknown or unenforceable policy
revisions must fail before Codex starts.

## Remaining runtime gates

1. Choose and pin the managed ACP executable/adapter revision, then define
   invocation-scoped credential capabilities.
2. Prove the sealed `deny_all` policy at that ACP boundary. Empty MCP alone is
   not proof of tool denial.
3. Add Huddles adapter wiring only after the runtime sink/event contract is
   reviewed; it must preserve `actor`, `execId`, `causationId`, and durable
   idempotency in the gateway.
4. If native app-server is enabled in managed Huddles, replace the local
   auto-accept approval behavior with enforceable denial and separately review
   its managed credential path. It remains local-microVM-only for now.

Until those gates exist, the managed Codex path must return an explicit
unsupported result rather than silently routing the request through OpenCode.
