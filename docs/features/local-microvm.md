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
  - "src/sandbox/mod.rs:reap_detached_leaves_no_zombie"
  - "scripts/smoke/libkrun.sh"
---

# Local microVM execution

The maintained local backend runs agents inside libkrun microVMs with a private rootfs, workspace fork and host-owned control channels. The `SandboxBackend`/`LiveSession` seam supports session operations. Read [architecture](../architecture.md) and the [substrate detail](../libkrun-sandbox.md), whose opening design history predates the implementation.

## Sub-features

- OCI image resolution, rootfs materialization/cache and private clones.
- Guest boot, vsock control, exec and PTY/server-agent transport.
- Workspace/auth environment fork and supervised VMM lifetime.

## How to get to it (user POV)

With the macOS libkrun toolchain, runner image and agent auth prepared, build/sign via `scripts/lk-build.sh`, then run `pillbox run --agent opencode`. Use `pillbox session list --json` to find the session and the session commands to drive/read it.

## Driving it with the repository harness

`cargo test --features libkrun sandbox::libkrun::boot::tests` verifies boot scripts, share ownership and quoted environment values without a VM boot. For a live, model-consuming proof, `scripts/smoke/run.sh libkrun` drives an edit, checks usage/output, pulls the result and verifies the original workspace stayed unchanged. The live smoke requires the image/auth/toolchain prerequisites above.

## Gotchas

- GitHub macOS CI compiles/links and runs non-VM tests; it cannot prove HVF boot.
- Fresh OCI exports still need Docker as image plumbing. That does not make the deprecated Docker execution backend a maintained alternative.
- A rebuild can replace the signed binary; use the build/sign script before a manual VM run.
- `run::start` also runs inside long-lived callers (dispatch, eval): a detached `__krun-vmm` or `__session-tailer` child must go to `sandbox::reap_detached`, never be dropped, or its exit leaves a zombie that signal-0 probes read as alive.
- The egress fence logs its DNS and TLS decisions to `PILLBOX_KRUN_EGRESS_LOG`, else to a per-run file under `~/.pillbox/krun/egress/` (pruned after 14 days, capped at 4 MiB). A foreground run prints the hosts it refused when it ends; a shared `PILLBOX_KRUN_EGRESS_LOG` gets no summary, since it mixes runs.
