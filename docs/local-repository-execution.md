# Bounded local repository execution

Status: implementation in progress on the SB-004 branch. The CLI and offline verifier VM are
implemented and locally verified. The bounded live provider, independent verifier, and successful
retry/conflict proofs pass. Production CLI cancellation and owner-crash probes also pass
during builder VM startup; final evidence review and CI remain pending. This is not a
released execution capability.

This is the Pillbox-owned runtime work for Huddles SB-004. It does not add Huddles workspace,
thread, packet, or event authority to Pillbox. Existing `pillbox.execution/2` requests retain their
closed wire shape and tool policy meanings. A separately versioned execution/3 request binds
the exact generic request, repository manifest, immutable input, and verifier before provisioning
or credential access. The command must not be promoted as supported until the end-to-end gate passes.

## Enforcement boundary

The closed runtime wire types are in [execution/mod.rs](../src/execution/mod.rs). Execution/3
requires `tool_policy: repository_files`, `execution_policy_revision: pillbox-local-files-v1`,
Codex 0.151.0 with adapter `pillbox/local-repository-v1`, and local microVM placement. Its manifest
contains the exact base, output identity, complete runner image ID, read/write and tool/secret
requirements, provider host, finite limits, and sealed Python verifier source. Request and manifest
digests use canonical JSON; duplicate delivery compares the complete original canonical request.
Only `pillbox_repository` read/remove/write operations and the opaque `pillbox:codex:default`
credential reference for purpose `model` are currently supported. The only guest provider host is
`chatgpt.com`. These are explicit admission restrictions, not fallback defaults.

The first source adapter resolves committed regular files using a full Git object ID, with ambient
configuration, replace objects and network fetching disabled. Dirty worktree bytes are not part of
this source adapter. The source adapter additionally caps commit objects at1MiB, each tree at4MiB,
total visited tree bytes at16MiB, tree entries at16,384 and depth at128. It preflights each exact
object before a nonrecursive read and rejects larger metadata without treating it as an empty tree.
Every Git child has verified16MiB allocation and32MiB mapping ceilings, bounded pack windows/cache,
no commit-graph or multi-pack-index acceleration, and the existing aggregate deadline/output caps.
These are object/working-buffer bounds, not a promise of a total process RSS limit. An unsupported
Git allocation guard fails before reading repository objects.
An independent verifier executes sealed source outside the builder's result
tree; the source and its runtime/output limits are included in `definition_digest`.

The first supported policy is a host-owned file broker. The agent VM receives no repository mount.
The sealed request binds an immutable, complete input snapshot, which the host verifies before
credential access or VM provisioning; a bounded
in-memory file tree owns edits. Only runtime-provided read, write, and remove tools can access that
tree. Native environment tools are disabled by the pinned Codex app-server configuration, whose
handler registration must be verified against the runner version. Prompts are not enforcement.
Codex 0.151.0 supports explicit `tool_mode: direct` model metadata. The new adapter preserves the
requested provider/model/effort and original metadata as provenance, changing only this tool
presentation field in its effective catalog. Code mode is disabled: its native `exec`/`wait`
wrappers lack a fail-closed pre-call hook and cannot satisfy the sealed total tool-call budget.
Every admitted function therefore reaches the host broker before execution. Real provider acceptance
of this profile is still a required end-to-end gate; there is no model or tool-profile fallback.

The file broker's input is a sorted sequence of regular files. Each entry has a canonical
repository-relative path, executable bit, and bytes. The snapshot digest is SHA-256 of canonical
JSON containing the sorted `{path, executable, sha256}` entries. Byte digests include the `sha256:`
prefix; JSON object keys use UTF-16 ordering and integer values only. This format has no host paths,
timestamps, owner IDs, directory entries, symlinks, devices, or `.git` contents. Unsupported input
fails closed rather than being silently omitted. Git commit identity is separately bound by the
runtime admission adapter; this tree digest identifies the exact input bytes; this Git adapter does not admit dirty input.

The first broker revision accepts ASCII paths only, with no absolute paths, traversal, empty
segments, backslashes, control characters, colons, wildcards, `.git` segments, or Pillbox-owned
secret exclusions. Non-ASCII input is an explicitly unsupported policy in this revision. No parent
directory grant is inferred. Read and write sets are separately sorted, unique exact file names.
Write-only permission must not return existing bytes. Removing a file requires its exact write
grant. A tool/operation pair is required independently of the path grant. Unsupported pairs fail
before launch. All mutations preserve the original immutable tree for a complete changed-path
manifest. A no-op write does not fabricate a changed path.

Native file tools use an explicit content codec: write arguments are `{path, executable, encoding,
content}`, where `encoding` is `utf8` or `base64`. Reads prefer UTF-8 for valid text and otherwise
return base64, with the same explicit encoding/content fields. The broker still receives exact
bytes; the codec grants no filesystem or process access. Both decoded byte limits and actual
serialized frame/evidence limits apply, including JSON escaping overhead.

Per-file, total snapshot, tool-call, and output byte limits are finite and enforced before allocation
or mutation. Failed operations do not partially mutate the tree. The sealed result includes every
changed path and the exact complete output tree digest. The producer must be stopped and reaped
before capturing results. A separate offline verifier VM receives a private copy of that exact
result and the trusted verifier definition; it receives no model credential. Build completion and
verification pass/fail are separate observations with separate positional SessionRefs.
The sealed Python evaluator runs after a fixed, root-owned bootstrap disables and verifies
process dumpability. Same-UID repository subprocesses cannot inspect its memory through proc/ptrace.
Trusted verifier source owns its evaluation semantics; importing repository code into its own
interpreter does not create a process isolation boundary.

## Credentials and transport

The invocation receives a fresh home containing generated configuration and allowlisted synthetic
authentication metadata. Never copy the user's Codex home, real ID token, access token, refresh
token, or unknown authentication fields. Only the existing host TokenStore may refresh provider
credentials. The VM receives a stub; the host-bound libkrun MITM releases an access token only to
the pinned provider host. Native HTTP/SSE transport may be pinned when the exact runner supports
it. A WebSocket policy must not be claimed until an authenticated upgrade gate is implemented.

## Lifecycle gate

An atomic durable claim binds canonical invocation ID, complete request digest, manifest digest,
input digest, and adapter/policy revision before any provisioning, credential read, or sampling.
An identical retry returns the same claim. Changed content conflicts. An invocation that loses
its supervisor becomes interrupted and never samples again. Cancellation is idempotent intent
until the supervisor has stopped the builder. Intent acknowledged before terminal commit wins
under a short transition lock; later cancellation observes the immutable terminal record. No resident daemon
or Huddles collaboration state is introduced.

The supported command must prove these properties with boundary tests and a real local microVM
execution, independent verifier, interruption, and retry evidence. A fixture response, model-written
JSON, successful CLI exit, or post-run diff is not proof of confinement. Until those gates pass,
this document describes unfinished work, not an available execution capability.

## CLI and evidence

The foreground interface is `pillbox execution execute --request request.json --repository /path/to/repo`.
The repository path is a host source locator; the sealed full commit and complete snapshot digest
identify the admitted bytes. `pillbox execution snapshot --repository /path/to/repo --commit FULL_OID`
computes that digest without credentials or execution. `pillbox execution status INVOCATION_ID` and
`pillbox execution cancel INVOCATION_ID` operate on the same resolved Pillbox state directory.
All four commands emit the existing JSON v1 envelope; execute/status/cancel contain an `execution`
record with `invocation_id`, `request_hash`, `status`, and `detail`. A recorded failure is a returned
execution state; malformed requests and invocation conflicts are CLI errors.

The libkrun backend alone declares `repository_execution`; backend selection never silently turns
this request into an unconfined run. Execute retains the ownership lock for its entire foreground
lifetime. A request observed after owner loss becomes `interrupted`, including a crash between
physical completion and terminal commit. It is never relaunched under that identity.

A successful detail contains separate admission, repository result, and verification records.
`verification.outcome` is `pass` or `fail`; `completed` is not a synonym for a passing verifier.
Every record binds request/manifest/input/output identities and existing SessionLog positions.
Raw native RPC frames, final text, patch, verifier report, and complete snapshot manifest are
content-addressed artifacts in the referenced session's existing BlobStore. The snapshot manifest
uses the same canonical sorted `{executable,path,sha256}` format as FileTree; each file's bytes are
also persisted under that digest in the same session. Returned references verify stored bytes and
synchronize them before terminal commit. A large model answer is an artifact, not unbounded
inline lifecycle state. No model-written object can serve as completion or verification evidence.

Running claims retain coarse progress references to the builder log, captured native frames,
result, and separate verifier session. Failure and owner-loss recovery preserve those references.
Raw verifier report bytes (including malformed or partial reports) are stored and linked before
interpretation, so a transport or parser failure remains inspectable without being called a verdict.

## Verification record — 2026-09-22

The [recorded offline proof](./local-repository-execution-offline-proof.json) contains the sealed
verifier definitions, exact report bytes/digests and observed results. It is a manual validation
record, not an execution/3 receipt. The signed local verifier probe used immutable image
`sha256:3af5c7cbb9e111e817960a30b01983f9b3891a7efb811a124c2a35d22c7126b2`.
Its five actual VM cases passed: privilege/filesystem/network restrictions and post-exec evaluator
protection, forged stdout, bounded output overflow, descendant timeout, and an escaped guest
session followed by whole-VM teardown. Each completed host stop/reap and left no observed owned
processes. Killing an active host supervisor also removed both observed owned groups within 89 ms.
The lifecycle gate requires both leader reap and process-group disappearance; a signal error or
leader exit alone is insufficient.

The guest exposed loopback and a down virtual dummy interface, with no device-backed NIC or
non-loopback route. TCP and UDP probes returned ENETUNREACH. The original fixture's stricter
interface-name assumption and the Darwin teardown race it subsequently exposed were retained as
failed observations; revised fixtures and the cleanup fix were tested independently.

These checks used synthetic data and no provider credentials. They establish the offline verifier
and owned-process boundaries, not model delivery, complete execution/3 integration, or Huddles
self-build. After explicit authorization, the [first production-CLI attempt](./local-repository-execution-live-attempt.json) on 2026-09-23 failed during managed
OAuth refresh (401 refresh_token_invalidated), before any VM or model invocation. The failed claim
and admission/failure evidence were preserved. Identical redelivery reused that record and changed
content conflicted without changing invocation/session bytes. A renewed managed login and an
explicit new invocation are required before live provider acceptance can be established.

The [portable smoke entry point](../scripts/smoke/repository-execution.sh) defaults to offline
preparation. Its [runbook](../scripts/smoke/repository-execution/README.md) separates explicit live
execution, artifact inspection and retry proof, preserving every attempt without automatic sampling.


### Native notification metadata

Codex 0.151.0's `ServerNotificationEnvelope` includes optional `emittedAtMs` (int64),
as defined in `app-server-protocol/src/protocol/common.rs` at upstream commit
`78c290807ce710180111df227df3b7a4fe845452`. The bounded adapter preserves this field
only on server notifications. It never controls lifecycle, ordering, deadline, or
SessionLog sequence; unknown envelope fields and timestamps on requests/responses
remain errors. The v2 live smoke exposed this decoder omission before `turn/start`.

## Successful bounded live proof

Attempt `v3` used the signed decoder-repaired binary from `66bfa59`, the same pinned
Codex 0.151.0 image, and the synthetic three-file fixture. It completed one native
`gpt-5.6-sol` turn with three granted file calls: read the task, read the answer,
and write `answer=42` to `src/answer.txt`. The ungranted canary was absent from
native evidence. A separate offline verifier session passed the complete result.
The captured patch reproduced the full expected snapshot, including executable bits.

Identical redelivery returned the same completed record. Changed content conflicted;
invocation and session fingerprints remained unchanged across both deliveries.
The exact binary, request, artifact and result identities and inspected checks are
recorded in [the live proof](./local-repository-execution-live-proof.json). Previous
failed attempts remain intact. This closes successful provider, patch, verifier and
retry proof. The separate startup cancellation and foreground-owner crash
observations follow below. It is not Huddles-commissioned self-build evidence.

## Bounded lifecycle observations

Two separately approved synthetic invocations exercised the production CLI during
builder startup. Each probe required a durable running admission and a single owned
`__krun-vmm` process-group leader in the exact foreground owner's descendant tree,
with no recorded result or verifier session. The probe saved only process metadata.

- Cancellation: two cancel requests returned the same invocation; the foreground
  owner returned `cancelled` and the observed VMM group disappeared. Identical
  delivery reused the cancelled record, and changed content conflicted without
  changing invocation/session fingerprints.
- Owner loss: the probe sent SIGKILL only to its recorded foreground child. The
  observed VMM group disappeared; status recovered `interrupted` with
  `invocation_owner_lost` and preserved admission evidence. Identical and changed
  delivery again preserved terminal and session bytes without relaunch.

[Lifecycle proof metadata](./local-repository-execution-lifecycle-proof.json) records
the process observations, terminal receipts and retry checks. These are host VMM
startup observations, not proof that a guest finished booting or a provider turn was
in flight at interruption. The separate v3 proof establishes model execution and
independent verification. The probes never repurposed the successful invocation.
