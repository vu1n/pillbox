# Bounded local repository execution

Status: implementation in progress. No public execution command currently admits this policy.

This is the Pillbox-owned runtime work for Huddles SB-004. It does not add Huddles workspace,
thread, packet, or event authority to Pillbox. Existing `pillbox.execution/2` requests retain their
closed wire shape and tool policy meanings. A separately versioned execution/3 request will bind
the exact generic request, repository manifest, immutable input, and verifier before provisioning
or credential access. The command remains unavailable until the end-to-end gate passes.

## Enforcement boundary

The first supported policy is a host-owned file broker. The agent VM receives no repository mount.
An immutable, complete input snapshot is materialized by the host before admission; a bounded
in-memory file tree owns edits. Only runtime-provided read, write, and remove tools can access that
tree. Native environment tools are disabled by the pinned Codex app-server configuration, whose
handler registration must be verified against the runner version. Prompts are not enforcement.

The file broker's input is a sorted sequence of regular files. Each entry has a canonical
repository-relative path, executable bit, and bytes. The snapshot digest is SHA-256 of canonical
JSON containing the sorted `{path, executable, sha256}` entries. Byte digests include the `sha256:`
prefix; JSON object keys use UTF-16 ordering and integer values only. This format has no host paths,
timestamps, owner IDs, directory entries, symlinks, devices, or `.git` contents. Unsupported input
fails closed rather than being silently omitted. Git commit identity is separately bound by the
runtime admission adapter; this tree digest identifies the exact input bytes, including dirty input.

The first broker revision accepts ASCII paths only, with no absolute paths, traversal, empty
segments, backslashes, control characters, colons, wildcards, `.git` segments, or Pillbox-owned
secret exclusions. Non-ASCII input is an explicitly unsupported policy in this revision. No parent
directory grant is inferred. Read and write sets are separately sorted, unique exact file names.
Write-only permission must not return existing bytes. Removing a file requires its exact write
grant. A tool/operation pair is required independently of the path grant. Unsupported pairs fail
before launch. All mutations preserve the original immutable tree for a complete changed-path
manifest. A no-op write does not fabricate a changed path.

Per-file, total snapshot, tool-call, and output byte limits are finite and enforced before allocation
or mutation. Failed operations do not partially mutate the tree. The sealed result includes every
changed path and the exact complete output tree digest. The producer must be stopped and reaped
before capturing results. A separate offline verifier VM receives a private copy of that exact
result and the trusted verifier definition; it receives no model credential. Build completion and
verification pass/fail are separate observations with separate positional SessionRefs.

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
until the supervisor has stopped the builder; terminal evidence is immutable. No resident daemon
or Huddles collaboration state is introduced.

The supported command must prove these properties with boundary tests and a real local microVM
execution, independent verifier, interruption, and retry evidence. A fixture response, model-written
JSON, successful CLI exit, or post-run diff is not proof of confinement. Until those gates pass,
this document describes unfinished work, not an available execution capability.
