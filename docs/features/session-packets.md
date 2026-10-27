---
id: session-packets
paths:
  - "src/checkpoint/**"
  - "src/commands/checkpoint.rs"
  - "proto/pillbox/session-packet-1.schema.json"
  - "src/agents/mod.rs"
entry:
  - "src/commands/checkpoint.rs"
  - "src/checkpoint/mod.rs:pack"
  - "src/checkpoint/mod.rs:unpack"
tests:
  - "tests/checkpoint.rs:pack_then_unpack_round_trips_with_versioned_json"
  - "tests/checkpoint.rs:unpack_restores_directory_modes_under_a_narrow_umask"
  - "src/checkpoint/tests.rs"
---

# Portable session packets

Engine callers such as Stratus can transfer a workspace and harness state between turns as a deterministic packet with a required digest on restore. These are hidden CLI subcommands available in the toolchain-free build. See [session packet](../session-packet.md).

## Sub-features

- Canonical manifest/schema and deterministic ustar/pax serialization.
- Credential exclusions, directory/symlink validation and size bounds.
- Digest-checked staged unpack with explicit nonempty-target handling.

## How to get to it (user POV)

`pillbox checkpoint pack --workspace DIR --harness claude_code --out FILE --json` reports the digest and excluded entries. Pass that digest to `pillbox checkpoint unpack --packet FILE --expect sha256:HEX --workspace EMPTY_DIR --json`; restored files match the packet.

## Driving it with the repository harness

`cargo test --no-default-features --test checkpoint` runs pack/unpack through the CLI, verifies versioned JSON, credential exclusion and restored directory modes. `cargo test --no-default-features checkpoint::tests` exercises format validation and malformed packet refusal.

## Gotchas

- Packet exclusions differ from workspace ingest: source key fixtures must survive a packet round trip, but harness credential files and their SQLite sidecars must not.
- The output packet cannot be inside either packed tree.
- The original packet-layout decision was proposed and awaiting review at retirement. Its [historical record](../archive/brief/README.md) remains unratified; documenting implemented behavior here does not ratify it.
