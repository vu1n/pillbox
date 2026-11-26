---
id: sandboxes
paths:
  - "src/commands/sandbox.rs"
  - "src/sandboxes.rs"
  - "src/sandbox/libkrun/sandbox_vm.rs"
  - "src/sandbox/exec_channel.rs"
entry:
  - "src/commands/sandbox.rs:dispatch"
  - "src/sandbox/libkrun/sandbox_vm.rs:spawn"
  - "src/sandbox/mod.rs:sandbox_target"
tests:
  - "src/sandbox/libkrun/sandbox_vm.rs:libkrun_declares_long_lived_exec"
  - "src/sandbox/libkrun/sandbox_vm.rs:agent_boot_script_hands_files_to_uid_1000_and_drops_before_exec_host"
  - "src/sandbox/libkrun/sandbox_vm.rs:exec_readiness_waits_for_the_channel_and_fails_on_vmm_exit_or_timeout"
  - "src/sandbox/libkrun/sandbox_vm.rs:libkrun_record_round_trips_to_a_target_and_destroy_removes_files_then_record"
  - "src/sandbox/exec_channel.rs:exec_channel_serve_and_run_deliver_both_streams_and_exit_code"
  - "src/sandbox/exec_channel.rs:exec_channel_kills_a_quiet_command_when_the_host_disconnects"
  - "src/sandbox/exec_channel.rs:exec_channel_exits_when_the_command_does_even_if_a_background_child_holds_stdout"
  - "src/sandbox/libkrun/sandbox_vm.rs:spawn_refuses_agents_that_are_not_env_forked_or_drivable_before_any_launch"
  - "src/sandbox/mod.rs:destroying_a_record_it_cannot_drive_drops_it_and_succeeds"
  - "src/sandbox/libkrun/mod.rs:an_abandoned_launch_removes_its_private_rootfs_and_nothing_outside_the_namespace"
  - "src/commands/sandbox.rs:exec_json_emits_started_output_per_stream_and_exit_code"
  - "src/commands/sandbox.rs:agent_exit_code_is_zero_in_json_only_when_run_finished_carries_the_outcome"
---

# Sandboxes

`pillbox sandbox spawn` boots a long-lived libkrun microVM that an orchestrator keeps
and drives with `sandbox exec` (one command per call, PTY-free) and `sandbox agent`
(one headless agent turn, normalized to contract events), then removes with
`sandbox destroy`. The commands are backend-blind: spawn goes through
`select_backend().spawn_sandbox`, and every other verb through the record's
`sandbox_target`. Only libkrun declares `long_lived_exec`; the deprecated Docker
backend refuses spawn; an old docker record (or one with an unknown backend or an
undecodable libkrun handle) resolves to a record-only target that refuses exec and whose
destroy drops the record, names what was left (`docker rm -f <container>`), and exits 0.

## Sub-features

- Spawn: the run path's own `launch_base` prologue (private rootfs, CoW secret-scrubbed
  workspace clone, egress fence, vault CA) into a guard-owned `PendingVm` (an error before
  the spawn drops the clones), a detached VMM spawned by the shared `spawn_reparented` in
  its own process group with a commit guard on the sandbox record, and a wait until the
  guest `exec-host` answers before the record is written. A failure after the VMM spawn
  goes through `SpawnedVm::commit` → `abandon_launch`, as a failed server bring-up does,
  which tears the VM and its files down. If the CLI itself dies first (Ctrl-C during the
  readiness wait), the VMM's commit guard removes the spec, shares, sockets and the private
  rootfs clone, then exits.
- `--agent`: the structured run's creds (`agent_launch_creds`). Only an agent that is
  vault-capable and has a stdout adapter is accepted (`claude` today; `sandbox_agents`);
  anything else is a usage error before any launch, so the guest never holds a real
  credential — stubs in the guest, the real token only on the VMM child's stdin.
  The guest runs commands as uid 1000. A bare sandbox gets an empty home and runs as root.
- Exec channel: framed request (argv, cwd = guest workspace) and stdout/stderr/exit
  frames over vsock port 1026, one connection per command (the codec is
  `attach::frame::{read_frame,write_frame}` with a 16 MiB cap). The command runs in its
  own process group: if the host closes the connection, the guest kills the group. Exit
  is sent when the command itself exits; a background child still holding stdout keeps
  running, and its later output is discarded.
- Destroy: the same `teardown_handle` as `session rm` on libkrun, then the record. A
  record pillbox can't drive is just dropped (see above).
- Agent exit code: plain `sandbox agent` exits with the turn's outcome; `--json` exits 0
  only when `run_finished` already carries it (a harness failure it didn't report, or no
  `run_finished` at all, exits non-zero).
- The record stores `guest_workspace`, the exec cwd. A record without it can be
  destroyed but not exec'd.

## How to get to it (user POV)

```sh
ID=$(pillbox sandbox spawn)                       # prints the id once exec answers
pillbox sandbox exec $ID -- sh -c 'echo hi > f'   # raw output, mirrored exit code
pillbox sandbox exec $ID --json -- cat f          # ExecStarted/ExecOutput/ExecExit JSONL
pillbox sandbox list --json                       # workspace_clone = where writes land
pillbox sandbox destroy $ID
```

The sandbox never writes the host workspace: read results from `workspace_clone`.

## How to drive it on the Mac

Needs macOS/HVF, a signed binary from `scripts/lk-build.sh`, the runner rootfs already
cached, and (for the agent channel) `pillbox auth login --agent claude`. Stop Docker
Desktop's daemon first to show nothing reaches Docker.

1. Exec: `ID=$(pillbox sandbox spawn)` prints an id. Then
   `pillbox sandbox exec $ID --json -- sh -c 'echo out; echo err >&2; exit 3'` emits
   `exec_started`, one `exec_output` with stream `stdout` and one with `stderr`, and
   `exec_exit` with code 3. `pillbox sandbox exec $ID -- sh -c 'echo x > marker'`
   followed by `pillbox sandbox exec $ID -- cat marker` prints `x`: a second exec sees
   the first one's file.
2. Agent: `ID=$(pillbox sandbox spawn --agent claude)`, then
   `pillbox sandbox agent $ID --json -- reply with the word pong` streams `run_started`,
   message deltas containing `pong`, and `run_finished`.
3. Destroy: `pillbox sandbox destroy $ID` exits 0, `pillbox sandbox list` no longer shows
   it, and `ps -axww | grep __krun-vmm` shows no VMM for its spec.

Offline (no VM): `cargo test --features libkrun sandbox_vm` and
`cargo test --no-default-features exec_channel` cover the boot script, VmSpec,
readiness wait, exec over a real exec-channel server, and destroy.

## Gotchas

- Writes land in the clone, not your working tree; copy results out of `workspace_clone`
  before `destroy`, which removes it.
- `--image` is a usage error: the rootfs is the pillbox's runner image
  (`[runner] image` / `PILLBOX_RUNNER_IMAGE`).
- An agent sandbox hands the home and workspace to uid 1000 with `chown -R` at boot.
  On macOS a read-only host file may refuse that (a warning on the VM console); it stays
  readable but not writable to the agent.
- `--agent cursor`/`pi` are refused: they aren't vault-capable, so their real key would
  sit in the guest. Add one only once it is env-forked and has a stdout adapter.
- `spawn` waits up to 120 s for the exec channel; a slow boot over a large workspace
  clone can hit that and is torn down.
