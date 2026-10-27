---
id: sessions
paths:
  - "src/session.rs"
  - "src/commands/session/**"
  - "src/events/**"
  - "src/gateway.rs"
  - "src/contract.rs"
  - "src/attach/**"
  - "src/cost.rs"
  - "proto/pillbox/v1/**"
entry:
  - "src/commands/session/mod.rs:dispatch"
  - "src/events/log.rs:SessionLog"
  - "src/gateway.rs"
tests:
  - "tests/session.rs:session_done_emits_completed_event"
  - "src/events/log.rs:concurrent_appenders_get_unique_contiguous_seqs"
  - "tests/attach.rs:direct_socket_attach_carries_agent_output"
---

# Session control, events and attachment

Users can inspect and drive sessions, replay attributed events, read artifacts/costs, and detach or attach to interactive execution. The local append log is the event authority. See [session log](../session-event-log.md), [attach transport](../attach-transport.md) and [commands](../commands.md).

## Sub-features

- Registry lookup, prefix resolution, status, completion, labels and expiry/pruning.
- Durable sequenced events, transcript adapters, blobs and read-side exporters.
- Input/send, local subscription/watch, PTY transport and detach/reattach.

## How to get to it (user POV)

`pillbox session list --json` locates an ID. `pillbox session info ID --json` inspects it; `pillbox session log ID` reads evidence and `pillbox session cost ID --json` reports measured usage. For a live server-agent session, `pillbox session send ID "next task"` drives the next turn; PTY agents receive bytes as-is and need a trailing newline/return to submit.

## Driving it with the repository harness

`cargo test --no-default-features --test session` exercises the CLI against planted local records and verifies completion events and prefix handling. `cargo test --no-default-features events::log::tests` verifies replay/recovery and concurrent sequence assignment. `cargo test --no-default-features --test attach direct_socket_attach_carries_agent_output -- --ignored` explicitly runs the ignored local PTY-host test and checks an output marker without a VM; use an idle machine because it is timing-sensitive.

## Gotchas

- The local registry/log is scoped to the selected pillbox; use the project directory or `--pillbox`.
- Local log/WS subscription does not imply a managed replay broker. Managed returned evidence is copied into the local log.
- Read-side replay and producer lifetime are separate concerns; a successful log read does not prove a detached producer is healthy.
