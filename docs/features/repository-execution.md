---
id: repository-execution
paths:
  - "src/commands/execution.rs"
  - "src/execution/mod.rs"
  - "src/execution/local.rs"
  - "src/execution/store.rs"
  - "src/execution/snapshot.rs"
  - "src/execution/files.rs"
  - "src/execution/native.rs"
  - "src/execution/protocol.rs"
  - "src/execution/verifier.rs"
  - "src/execution/probe.rs"
  - "src/execution/evidence.rs"
  - "src/sandbox/libkrun/repository/**"
  - "src/sandbox/libkrun/repository.rs"
  - "scripts/smoke/repository-execution/**"
  - "scripts/smoke/repository-execution.sh"
entry:
  - "src/commands/execution.rs:dispatch"
  - "src/execution/store.rs:InvocationStore"
  - "src/execution/local.rs:execute"
tests:
  - "src/execution/store.rs:duplicate_never_gets_ownership_and_changed_request_conflicts"
  - "src/execution/store.rs:owner_loss_is_interrupted_even_before_first_launch"
  - "scripts/smoke/repository-execution/test_smoke.py"
---

# Bounded repository execution

An engine caller binds one invocation to an exact committed repository, bounded file/tool capabilities and verifier evidence. Exact retries return the original record; they do not sample again. The local execution/3 release gate remains pending. See [repository execution](../local-repository-execution.md).

## Sub-features

- Committed-tree snapshot digest and sealed request validation.
- Durable claim, status/cancellation and lost-owner recovery.
- Separate builder/verifier VMs, bounded artifacts and standalone offline verifier probes.

## How to get to it (user POV)

`pillbox execution snapshot --repository DIR --commit FULL_OID` reports the immutable snapshot digest. With a valid sealed request, `pillbox execution execute --request request.json --repository DIR` runs foreground; another process can call `pillbox execution status ID` or `pillbox execution cancel ID`.

## Driving it with the repository harness

`python3 -m unittest discover -s scripts/smoke/repository-execution -p "test_*.py" -v` verifies fixture preparation, integrity checks and evidence inspection offline. `cargo test --no-default-features execution::store::tests` checks duplicate/conflict, owner-loss and cancellation behavior with durable local files. Live invocation needs the separate prerequisites and explicit live mode in the [smoke guide](../../scripts/smoke/repository-execution/README.md); offline fixture success is not a live receipt.

## Gotchas

- Changed content under an existing invocation ID conflicts; owner loss becomes interrupted rather than authorizing a replacement sampler.
- A completed status alone does not prove the edit/verifier/artifact bindings. The smoke inspector writes `checked.json` only after checking them.
- Repository execution is a libkrun capability; the toolchain-free tests cannot establish VM isolation or teardown.
