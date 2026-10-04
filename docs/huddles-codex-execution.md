# Huddles Codex execution boundaries

## Local text invocation (`pillbox.text/1`)

The local text producer is a distinct closed contract from managed
`pillbox.execution/2` and repository `pillbox.execution/3`. It runs one fresh,
tool-free Codex app-server turn in an owned libkrun microVM. Huddles supplies one
sealed rendered input and canonical invocation ID; Pillbox never owns the Huddles
conversation, agent principal, packet, or follow-up schedule.

`pillbox text execute --request FILE`, `pillbox text status INVOCATION_ID`, and
`pillbox text cancel INVOCATION_ID` use a separate durable claim namespace. The
execute request has the exact fields below; unknown fields fail admission:

```text
contract_version: "pillbox.text/1"
session_ref: { session_id }
invocation_id
idempotency_key: invocation_id
rendered_input
rendered_input_hash: sha256 of exact UTF-8 input
tool_policy: "deny_all"
execution: {
  transport: { harness: "codex", transport: "app_server",
               harness_version: "0.156.1", adapter_revision: "pillbox/local-text-v1" },
  requested: { provider: "openai", model: "gpt-6-luna", profile: "luna",
               reasoning_effort: "low" | "medium" | "high" },
  placement: "local_microvm", context_renderer_revision
}
execution_policy_revision: "pillbox-local-text-v1"
output_format: { type: "text", retry_count: 0 }
runtime: {
  runner_image_id, effective_model_catalog_digest,
  credential_ref: "pillbox:codex:default", network_hosts: ["chatgpt.com"],
  limits: { timeout_ms, max_final_text_bytes, max_frame_bytes,
            max_evidence_bytes }
}
```

Input is 1–524288 UTF-8 bytes. Limits are positive and bounded by 3600000 ms,
1048576 final-text bytes, 12582912 frame bytes and 67108864 evidence bytes;
the frame limit cannot exceed the evidence limit. The image ID is a full
lowercase SHA-256 digest and the catalog digest must match the embedded pinned
GPT-6 catalog. No tools or ambient workspace are exposed. Every server request,
including a dynamic tool or approval, fails the invocation. An empty or absent
final assistant answer fails rather than producing a chat reply.

Admission persists the complete canonical request before provisioning,
credentials or sampling. An identical retry returns its existing state; changed
content conflicts. Lost ownership interrupts an incomplete claim, with no
automatic resampling. Cancellation is intent until the owned process stops.
Completed output binds the exact request, native thread/turn, bounded final text,
its content-addressed artifact, raw native RPC artifact and local SessionLog
range. The requested model is distinct from a provider-observed served model;
the latter is unavailable unless supported by positional native evidence. This
claim provides at-most-once dispatch, not an exactly-once sampling guarantee.

## Managed Codex boundary

Status: sealed envelope only. No ACP adapter exists: the Rust and Cloudflare
ACP spikes were deleted on 2026-10-04 because nothing called them; the design
below is what a future adapter must satisfy. The existing managed
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

ACP is a process/event boundary, not an orchestration boundary. An adapter
should borrow Buzz's substrate ideas—bounded NDJSON, correlated requests,
cancellation cleanup, crash interruption, and respawn for a later invocation—
but omit its relay, durable prompt queue, and agent-pool claim scheduling. A
second active turn returns `runtime_busy`; it is never queued. There is no
automatic ACP/app-server fallback.

For ACP, `session/new` receives only the policy-derived `mcpServers: []`, and
`prompt` receives exactly the sealed `rendered_input`. The injected event sink
gets `session_ref`, `invocation_id`, the computed execution digest, and policy
revision for attribution. No mutable ACP context can override the sealed HCP
packet, and the adapter emits neither HCP nor WorkEvent records. Huddles keeps
ownership of HCP/WorkEvent orchestration, retries, sequencing, cancellation
intent, and execution identity.

The adapter checks one ACP result against the sealed `output_format` schema and
returns a safe structured-output failure without echoing provider content. It
does not retry inside ACP; any retry decision remains Huddles orchestration.

The deleted spikes (`src/sandbox/acp.rs`, `cloudflare-spike/src/acp_turn.ts`,
commit bf38f5b) remain in git history as a reference for that lifecycle.

### ACP v1 session primitives

ACP v1 stabilised more than the two calls the spike drives. Each primitive has
a fixed stance here so nobody "helpfully" adopts one and moves orchestration
across the boundary:

| ACP primitive                        | Stance in this boundary                                                                                                                                                                                               |
| ------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `initialize`, `session/new`          | Used. One session per invocation; `session/new` receives only the policy-derived `mcpServers: []`.                                                                                                                    |
| `session/prompt`, `session/update`   | Used. The prompt is exactly the sealed `rendered_input`; update notifications are normalised into §0 events and never re-exposed as ACP.                                                                             |
| `session/cancel`                     | Used as Huddles' cancellation intent. The invocation is `cancelled` only when the turn reports it.                                                                                                                    |
| `session/request_permission`         | Not answered by the adapter. Under `deny_all` no tool is advertised, so none arises. A future permission request maps to a typed Huddles `requires` entry (reserved as `input_required` in the lifecycle mapping below), never to an adapter auto-accept. |
| `session/fork`                       | Never. A Huddles fork is a compile-pipeline fork that seals a new packet and invocation; a copied ACP session would carry mutable context past the seal.                                                              |
| `session/load` (resume)              | Never. Continuation is Huddles orchestration: a new invocation whose packet names its lineage. The runtime never resumes a terminal invocation.                                                                        |
| `session/list`, `session/close`      | Not used as signals. Cleanup and respawn are adapter-internal and only ever prepare a later invocation.                                                                                                               |
| Elicitation, MCP-over-ACP            | Not used. Huddles owns every human-facing question and the tool policy; the adapter neither forwards a question to a person nor mounts an MCP server the packet did not seal.                                          |

## Versioned boundary

Pillbox exposes the private `pillbox.execution/2` contract in
`cloudflare-spike/src/codex_execution.ts` as its only managed Huddles RPC.
The historical OpenCode `ensureSession`/`invokeSession` adapter and its
local-test entrypoint were deleted on 2026-10-04.

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
reference. The boundary validator accepts that shape. A separate capability
check refines it to native Codex over `app_server`; `acp` still validates as a
transport, but nothing in Pillbox drives it.

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

## Huddles integration contract

Huddles remains responsible for constructing and authorizing
`InvocationExecution`, selecting and sealing the execution policy revision,
workspace and effect identities, scheduling, retries, cancellation intent, and
interpreting the result. Its Pillbox adapter calls the v2 private method with
the rendered input, input hash, output format, and sealed
`execution_policy_revision`; it should not derive a second execution profile or
turn the policy revision into Pillbox orchestration.

Pillbox is responsible for validating the boundary, enforcing the known policy
before spawning Codex, launching the pinned app-server, normalizing
notifications into §0 events, stamping `a:codex` and invocation correlation,
sequencing/cancellation, and returning terminal evidence. This envelope carries
no credential field; any future credential capability must be specified and
scoped before the runtime adapter resolves one. Unknown or unenforceable policy
revisions must fail before Codex starts.

### Lifecycle mapping

The result `status` union is deliberately shape-compatible with the MCP tasks
lifecycle (SEP-2663) so a tasks-backed runtime can sit behind the same Huddles
adapter with no wire change. Huddles fixes the mapping in
`docs/hcp/COMPILATION.md` §9.1 and pins the status set with a contract test;
Pillbox conforms to it rather than the reverse:

```text
running      -> working         retry_after_ms is the poll interval
completed    -> completed
failed       -> failed
interrupted  -> failed          runtime lost; retry is a Huddles decision
cancelled    -> cancelled       only after cancel intent was received
conflict     -> (none)          idempotency conflict, the packet never ran
input_required  reserved        not emitted until Huddles specifies the typed request
```

Cancellation is intent, not a state transition: `cancel` is idempotent and is
answered with a status result. Terminal results are final; a later turn on the
same session is a new invocation.

## Remaining runtime gates

1. Choose and pin the managed ACP executable/adapter revision, then define
   invocation-scoped credential capabilities.
2. Prove the sealed `deny_all` policy at that ACP boundary. Empty MCP alone is
   not proof of tool denial.
3. Cut Huddles over to the generic execution methods while keeping actor,
   collaboration order, retry intent, and visibility policy in Huddles.
4. If native app-server is enabled in managed Huddles, replace the local
   auto-accept approval behavior with enforceable denial and separately review
   its managed credential path. It remains local-microVM-only for now.

Until those gates exist, the managed Codex path must return an explicit
unsupported result rather than silently routing the request through OpenCode.
