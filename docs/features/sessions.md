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
  - "src/events/capture.rs:a_fifo_or_device_capture_is_refused_without_blocking"
  - "src/events/opencode.rs:follow_reader_refuses_a_symlink_or_fifo_capture"
  - "src/events/opencode.rs:an_oversized_sse_line_or_frame_is_skipped_and_later_frames_drain"
  - "src/events/transcripts/tailer.rs:a_swapped_parent_directory_stops_the_best_effort_tail"
  - "src/events/transcripts/tailer.rs:an_operator_named_symlink_is_followed"
---

# Session control, events and attachment

Users can inspect and drive sessions, replay attributed events, read artifacts/costs, and detach or attach to interactive execution. The local append log is the event authority. See [session log](../session-event-log.md), [attach transport](../attach-transport.md) and [commands](../commands.md).

## Sub-features

- Registry lookup, prefix resolution, status, completion, labels and expiry/pruning.
- Durable sequenced events, transcript adapters, blobs and read-side exporters.
- Input/send, local subscription/watch, PTY transport and detach/reattach.
- Typed in-process session calls (`send`, `stream::wait_idle`, `workspace_of`, `score`, `rehydrate_result`) that the CLI handlers render; their `--json` output is one client, not the API.

## How to get to it (user POV)

`pillbox session list --json` locates an ID. `pillbox session info ID --json` inspects it; `pillbox session log ID` reads evidence and `pillbox session cost ID --json` reports measured usage. For a live server-agent session, `pillbox session send ID "next task"` drives the next turn; PTY agents receive bytes as-is and need a trailing newline/return to submit.

## Driving it with the repository harness

`cargo test --no-default-features --test session` exercises the CLI against planted local records and verifies completion events and prefix handling. `cargo test --no-default-features events::log::tests` verifies replay/recovery and concurrent sequence assignment. `cargo test --no-default-features --test attach direct_socket_attach_carries_agent_output -- --ignored` explicitly runs the ignored local PTY-host test and checks an output marker without a VM; use an idle machine because it is timing-sensitive.

## Gotchas

- The local registry/log is scoped to the selected pillbox; use the project directory or `--pillbox`.
- Local log/WS subscription does not imply a managed replay broker. Managed returned evidence is copied into the local log.
- Read-side replay and producer lifetime are separate concerns; a successful log read does not prove a detached producer is healthy.
- Every file a guest writes (opencode/codex event captures, structured-stdout captures, transcripts) is opened through `events::capture::open_guest_file` (no symlink follow, no FIFO block, regular files only) and read with `read_line_capped`; a line or SSE frame past `MAX_CAPTURE_LINE_BYTES` (the 8 MiB §0 record cap) is skipped with a warning, not buffered. dispatch/eval drain these in-process, so an unbounded read there would take down the orchestrator.
- `O_NOFOLLOW` guards only a path's last component, and the transcript `Tailer` re-opens its path on every pump, so a guest-discovered transcript is a `TranscriptSource::Guest` bound to the `(device, inode)` discovery saw: a pump that opens another file (a parent directory swapped for a symlink) stops the best-effort tail with a warning and fails the durable one. `session transcript FILE --follow` names an operator path, so it passes `TranscriptSource::Operator` and opens FILE as named, symlinks included.
