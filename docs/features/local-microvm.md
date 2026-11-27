---
id: local-microvm
paths:
  - "src/sandbox/mod.rs"
  - "src/commands/run.rs"
  - "src/sandbox/libkrun/**"
  - "src/docker.rs"
  - "src/startup.rs"
  - "src/commands/sandbox.rs"
  - "src/sandboxes.rs"
  - "runner/**"
  - "build.rs"
entry:
  - "src/sandbox/mod.rs:select_backend"
  - "src/commands/run.rs:start"
  - "src/sandbox/libkrun/session.rs:prepare_launch"
  - "src/sandbox/libkrun/rootfs.rs"
tests:
  - "src/sandbox/libkrun/boot.rs:boot_channel_binds_share_to_exec_and_writes_script"
  - "src/sandbox/libkrun/mod.rs:rootfs_tar_extraction_preserves_sticky_world_writable_mode"
  - "src/commands/run.rs:json_envelope_is_the_same_for_every_lifecycle"
  - "src/sandbox/mod.rs:dropping_a_detached_child_reaps_it"
  - "src/sandbox/mod.rs:an_explicitly_stopped_detached_child_is_waited_once"
  - "src/sandbox/libkrun/http.rs:an_endless_body_errors_at_the_cap"
  - "src/sandbox/libkrun/http.rs:a_silent_guest_errors_at_the_timeout"
  - "src/sandbox/libkrun/http.rs:a_guest_that_reads_a_trickle_errors_at_the_deadline"
  - "src/sandbox/libkrun/rootfs_backing.rs:identity_survives_a_reboot_renumbering_the_volume"
  - "scripts/smoke/libkrun.sh"
---

# Local microVM execution

The maintained local backend runs agents inside libkrun microVMs with a private rootfs, workspace fork and host-owned control channels. The `SandboxBackend`/`LiveSession` seam supports session operations. Read [architecture](../architecture.md) and the [substrate detail](../libkrun-sandbox.md), whose opening design history predates the implementation.

## Sub-features

- OCI image resolution, rootfs materialization/cache and private clones.
- Guest boot, vsock control, exec and PTY/server-agent transport.
- Workspace/auth environment fork and supervised VMM lifetime.
- One prologue (`session.rs:launch_base`: disk preflight, rootfs, scrubbed workspace clone, guest env, vault CA) and one `__krun-vmm` command builder (`session.rs:vmm_command`, specs written only by `write_vm_spec`) shared by the PTY, server, structured, grader, repository, `auth login` and `sandbox spawn` launches. The clones, creds share and spec are guard-owned (`PendingVm`) until a launch keeps them, so an error before the spawn leaves nothing. A foreground VMM (`run_foreground_attached`, used by `run` and `auth login`) stays in the CLI's process group so a Ctrl-C in the boot window can't orphan it; a reparented one (`spawn_reparented`) gets its own group and a commit guard on the record its launch writes, and a failed reparented launch goes through `SpawnedVm::commit` → `abandon_launch`. If the CLI dies before the record exists, the guard removes the spec, shares, sockets and the private rootfs clone (the host home reaches it as `PILLBOX_COMMIT_HOME`, since the VMM's `HOME` is the guest's). The guest is still live then, so every rootfs removal is fd-relative and never follows a link (`rootfs::remove_tree_nofollow`). A supervised VMM (structured runs, the grader, repository invocations) is tied to an owner socket instead. Swap pairs reach a VMM only on its stdin; a failure to encode or write them is an error, and a VMM already spawned is killed.

## How to get to it (user POV)

With the macOS libkrun toolchain, runner image and agent auth prepared, build/sign via `scripts/lk-build.sh`, then run `pillbox run --agent opencode`. Use `pillbox session list --json` to find the session and the session commands to drive/read it.

## Driving it with the repository harness

`cargo test --features libkrun sandbox::libkrun::boot::tests` verifies boot scripts, share ownership and quoted environment values without a VM boot. For a live, model-consuming proof, `scripts/smoke/run.sh libkrun` drives an edit, checks usage/output, pulls the result and verifies the original workspace stayed unchanged. The live smoke requires the image/auth/toolchain prerequisites above.

## Gotchas

- GitHub macOS CI compiles/links and runs non-VM tests; it cannot prove HVF boot.
- Fresh OCI exports still need Docker as image plumbing. That does not make the deprecated Docker execution backend a maintained alternative.
- A rebuild can replace the signed binary; use the build/sign script before a manual VM run.
- `run::start` also runs inside long-lived callers (dispatch, eval): a detached `__krun-vmm` or `__session-tailer` child is spawned as a `sandbox::DetachedChild` (never a bare `Child`): dropping it hands it to a waiter thread, and the failure paths consume it (`kill_and_wait`, or `into_child` for `stop_spawned_vmm`), so an exit never leaves a zombie that signal-0 probes read as alive.
- The egress fence logs its DNS and TLS decisions to `PILLBOX_KRUN_EGRESS_LOG`, else to a per-run file under `~/.pillbox/krun/egress/` (pruned after 14 days, capped at 4 MiB). A foreground run prints the hosts it refused when it ends; a shared `PILLBOX_KRUN_EGRESS_LOG` gets no summary, since it mixes runs.
- The host's HTTP client to the guest server (`sandbox/libkrun/http.rs`; opencode or the codex app-server bridge) treats the guest as untrusted: a reply is capped at `MAX_RESPONSE_BYTES` (16 MiB) and a whole exchange, request write included, at `RESPONSE_TIMEOUT` (60 s), because dispatch/eval call it in-process and an unbounded read would take down the orchestrator. Its tests need the libkrun feature (a stub `libkrun.so` is enough on Linux); the `--no-default-features` gate does not run them.
- The macOS rootfs sparse bundle (`~/.pillbox/krun/case-sensitive-rootfs.sparsebundle`) is admitted by its inode, by sitting on the krun dir's live filesystem, and by the random token in the mounted volume's marker. It is never admitted by a stored `st_dev`, because macOS renumbers volume device numbers across reboots. Older `rootfs-backing.identity.json` files still carry `image_device`; it is read and ignored.
