# Architecture map

**Trust contract:** each claim below is tagged `[verified <date>]` (read in the
code) or `[unverified]` (carried from memory/older docs — confirm before relying
on it). If code disagrees with this map, **code wins** — fix the map in the same
change. The point of this file is to stop agents asserting structure they haven't
checked.

pillbox = **a self-contained bundle (workspace + code + vault + config)** that an
orchestrator runs agents against, inside a **local microVM** (libkrun) with a
**host-side credential/egress vault** in front of the guest's network.

## Subsystem map

| Subsystem | Canonical doc | One line | Status |
|---|---|---|---|
| Backend / substrate | docs/substrate-plane.md, docs/libkrun-sandbox.md | the microVM that runs the agent; the `SandboxBackend`/`LiveSession` seam | libkrun live; docker backend slated for deletion (ADR-002) |
| Vault (creds + egress) | docs/vault.md | host-side MITM that swaps stub→real creds + fences egress | live; OAuth re-aimed at the broker model (ADR-004) |
| Sessions / §0 | docs/session-event-log.md | durable event log, drive (`send`) + read (`subscribe`/`watch`) surface | `[unverified here]` |
| Snapshots | docs/config.md, AGENTS.md | rustic repo, push/pull, bookmarks | `[unverified here]` |
| Dispatch / eval | docs/dispatch.md, docs/eval.md | fork-k verified workers; the eval runner | `[unverified here]` |
| Managed / CF tier | docs/managed-tier.md, docs/gateway.md | the §0-gateway Durable Object; not yet a `run` backend | aspirational |

## Entanglements that bite

The cross-cutting dependencies that *look* separable but aren't. These are the
ones that produced wrong "authoritative" answers — check here before claiming a
change is clean.

### libkrun rides on docker for its guest image `[verified 2026-06-20]`
`materialize_rootfs` (src/sandbox/libkrun/mod.rs:~626) builds libkrun's virtio-fs
root by `docker pull` → `docker create` → `docker export | tar -x`, cached under
`~/.pillbox/krun/rootfs/<image-id>`. **So a libkrun run requires the docker daemon
today.** Consequence: "delete docker" ≠ "remove the docker dependency" — see the
two halves below.

### `crate::docker` is two fused things `[verified 2026-06-20]`
- **The backend** — `DockerBackend` / `DockerLiveSession` (run the agent in a
  container). This is what ADR-002 deletes.
- **Image/build plumbing** — `resolve_runner_image` / `default_runner_image`
  (src/docker.rs), used by libkrun (above), `doctor.rs`, `main.rs:~847`. This is
  kept and renamed `crate::oci`.
- Also: `auth login` runs the agent's OAuth flow in a container via
  `docker::run_interactive` / `check_ready_for` (src/agents/mod.rs:~394,412) —
  needs porting to libkrun before docker-the-tool is fully gone.

### The backend seam `[verified 2026-06-20]`
One backend = one impl of two traits in `src/sandbox/mod.rs`:
`SandboxBackend` (`run` + `capabilities` + `id`) and `LiveSession`
(`send`/`attach`/`spawn_log_tailer`/`http`/`workspace_path`/`ingest`/`kill`,
capability-gated via `Caps`). The **only** two places that branch on backend are
`select_backend()` and `live_session()`. Behavior also forks at ~38
`#[cfg(feature = "libkrun")]` gates — collapsing those (libkrun-only) is the
structural win when the docker backend is deleted.

### libkrun launch shape `[verified 2026-06-20]`
`krun_create_ctx` → `set_vm_config` → `set_root(dir)` (virtio-fs root, *not* a
disk) → `add_virtiofs` shares → `add_net_unixstream` (egress; needs libkrun built
`NET=1`) → `add_vsock_port2` (control/attach) → `set_exec` → `start_enter` (boots,
never returns, forks a `__krun-vmm` subprocess). Kernel is bundled (libkrunfw).
The egress L2 frames go to a host socketpair where a **smoltcp** userspace stack +
the vault MITM live (src/sandbox/libkrun/egress.rs).

## Open structural debts

- **Daemonless OCI image pull** — replace `docker create/export` so libkrun
  doesn't need the docker daemon (ADR-002 follow-up).
- **auth-in-libkrun** — `auth login` runs in a docker container today (ADR-002
  follow-up).
- **No-backend build** — `--no-default-features` currently compiles because the
  docker backend is the fallback; deleting it means resolving what the
  toolchain-free / in-sandbox-guest build does.
- **Spec/code drift** — several big docs (vnext, managed-tier, the optimization
  family) are aspirational, not current. They belong in docs/archive/ with a
  clear marker (ADR-005).
