---
id: session-packets
paths:
  - "crates/packet/**"
  - "crates/credentials/**"
  - "src/commands/checkpoint.rs"
  - "src/agents/mod.rs"
entry:
  - "crates/packet/src/main.rs"
  - "crates/packet/src/cli.rs:run"
  - "crates/packet/src/lib.rs:pack"
  - "crates/packet/src/lib.rs:unpack"
  - "src/commands/checkpoint.rs"
tests:
  - "tests/checkpoint.rs:pack_then_unpack_round_trips_with_versioned_json"
  - "tests/checkpoint.rs:unpack_restores_directory_modes_under_a_narrow_umask"
  - "crates/packet/src/tests.rs"
  - "crates/packet/tests/golden.rs:packet_from_29d5306_unpacks_and_repacks_byte_identical"
---

# Portable session packets

Engine callers such as Stratus can transfer a workspace and harness state between turns as a deterministic packet with a required digest on restore. The packet is its own workspace crate, `pillbox-packet` (`crates/packet/`), with a standalone `pillbox-packet pack|unpack` binary that builds without the rest of pillbox; `pillbox checkpoint pack|unpack` are the same subcommands as hidden CLI subcommands in the toolchain-free build. See [session packet](../../crates/packet/session-packet.md).

## Sub-features

- Canonical manifest/schema and deterministic ustar/pax serialization.
- Credential exclusions, `--skip-dir` for regenerable workspace directories (Stratus skips `node_modules`), directory/symlink validation and size bounds.
- Digest-checked staged unpack with explicit nonempty-target handling.

## How to get to it (user POV)

`pillbox-packet pack --workspace DIR --harness claude_code --out FILE --json` (or `pillbox checkpoint pack …` with the same flags) reports the digest and excluded entries. Pass that digest to `pillbox-packet unpack --packet FILE --expect sha256:HEX --workspace EMPTY_DIR --json`; restored files match the packet. Build the binary alone with `cargo build -p pillbox-packet`.

## Driving it with the repository harness

`cargo test --no-default-features --test checkpoint` runs pack/unpack through the CLI, verifies versioned JSON, credential exclusion and restored directory modes. `cargo test -p pillbox-packet` exercises format validation and malformed packet refusal, and unpacks and repacks a packet pillbox 29d5306 wrote to the same bytes (`crates/packet/tests/fixtures/golden-29d5306/`).

## Gotchas

- Packet exclusions differ from workspace ingest: source key fixtures must survive a packet round trip, but harness credential files and their SQLite sidecars must not.
- The output packet cannot be inside either packed tree.
- Credential file names come from the `pillbox-credentials` crate (`crates/credentials/`). `AgentSpec::cred_sentinel` is an `AgentCredential`, which only that crate's `agent_credentials!` table can make, and the same table generates `AGENT_CREDENTIALS`; so every agent's credential file is excluded from the harness tree by construction.
- The golden fixture pins the format: a change that alters its repacked bytes changes the format and needs a new format id.
- The original packet-layout decision was proposed and awaiting review at retirement. Its [historical record](../archive/brief/README.md) remains unratified; documenting implemented behavior here does not ratify it.
