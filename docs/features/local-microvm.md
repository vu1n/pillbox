---
id: local-microvm
paths:
  - "src/sandbox/mod.rs"
  - "src/sandbox/libkrun/**"
  - "src/docker.rs"
  - "src/startup.rs"
  - "src/commands/sandbox.rs"
  - "src/sandboxes.rs"
  - "runner/**"
  - "build.rs"
entry:
  - "src/sandbox/mod.rs:select_backend"
  - "src/sandbox/libkrun/session.rs:prepare_launch"
  - "src/sandbox/libkrun/rootfs.rs"
tests:
  - "src/sandbox/libkrun/boot.rs:boot_channel_binds_share_to_exec_and_writes_script"
  - "src/sandbox/libkrun/mod.rs:rootfs_tar_extraction_preserves_sticky_world_writable_mode"
  - "scripts/smoke/libkrun.sh"
---

# Local microVM execution

The maintained local backend runs agents inside libkrun microVMs with a private rootfs, workspace fork and host-owned control channels. The `SandboxBackend`/`LiveSession` seam supports session operations. Read [architecture](../architecture.md) and the [substrate detail](../libkrun-sandbox.md), whose opening design history predates the implementation.

## Sub-features

- OCI image resolution, rootfs materialization/cache and private clones.
- Guest boot, vsock control, exec and PTY/server-agent transport.
- Workspace/auth environment fork and supervised VMM lifetime.
- One prologue (`session.rs:launch_base`: disk preflight, rootfs, scrubbed workspace clone, guest env, vault CA) and one `__krun-vmm` command builder (`session.rs:vmm_command`, specs written only by `write_vm_spec`) shared by the PTY, server, structured, grader, repository, `auth login` and `sandbox spawn` launches. The clones, creds share and spec are guard-owned (`PendingVm`) until a launch keeps them, so an error before the spawn leaves nothing. A foreground VMM (`run_foreground_attached`, used by `run` and `auth login`) stays in the CLI's process group so a Ctrl-C in the boot window can't orphan it; a reparented one (`spawn_reparented`) gets its own group and a commit guard on the record its launch writes, and a failed reparented launch goes through `SpawnedVm::commit` → `abandon_launch`. If the CLI dies before the record exists, the guard removes the spec, shares, sockets and the private rootfs clone (the host home reaches it as `PILLBOX_COMMIT_HOME`, since the VMM's `HOME` is the guest's). A supervised VMM (structured runs, the grader, repository invocations) is tied to an owner socket instead. Swap pairs reach a VMM only on its stdin; a failure to encode or write them is an error, and a VMM already spawned is killed.

## How to get to it (user POV)

With the macOS libkrun toolchain, runner image and agent auth prepared, build/sign via `scripts/lk-build.sh`, then run `pillbox run --agent opencode`. Use `pillbox session list --json` to find the session and the session commands to drive/read it.

## Driving it with the repository harness

`cargo test --features libkrun sandbox::libkrun::boot::tests` verifies boot scripts, share ownership and quoted environment values without a VM boot. For a live, model-consuming proof, `scripts/smoke/run.sh libkrun` drives an edit, checks usage/output, pulls the result and verifies the original workspace stayed unchanged. The live smoke requires the image/auth/toolchain prerequisites above.

## Gotchas

- GitHub macOS CI compiles/links and runs non-VM tests; it cannot prove HVF boot.
- Fresh OCI exports still need Docker as image plumbing. That does not make the deprecated Docker execution backend a maintained alternative.
- A rebuild can replace the signed binary; use the build/sign script before a manual VM run.
- The egress fence logs its DNS and TLS decisions to `PILLBOX_KRUN_EGRESS_LOG`, else to a per-run file under `~/.pillbox/krun/egress/` (pruned after 14 days, capped at 4 MiB). A foreground run prints the hosts it refused when it ends; a shared `PILLBOX_KRUN_EGRESS_LOG` gets no summary, since it mixes runs.
