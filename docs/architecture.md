# Architecture map

**Trust contract:** each claim below is tagged `[verified <date>]` (read in the
code) or `[unverified]` (carried from memory/older docs — confirm before relying
on it). If code disagrees with this map, **code wins** — fix the map in the same
change. The point of this file is to stop agents asserting structure they haven't
checked.

pillbox = **a durable coding-agent session + self-contained bundle** (workspace
+ code + vault + config). Locally, the agent runs inside a libkrun microVM with
a host-side credential/egress vault. In the experimental managed placement,
Cloudflare Sandbox owns container lifecycle, D1/R2 hold bounded execution
metadata/evidence, and the caller keeps its §0 log local.

## Subsystem map

| Subsystem | Canonical doc | One line | Status |
|---|---|---|---|
| Backend / substrate | docs/substrate-plane.md, docs/libkrun-sandbox.md | the microVM that runs the agent; the `SandboxBackend`/`LiveSession` seam | libkrun live; Docker backend deleted `[verified 2026-11-27]` |
| Vault (creds + egress) | docs/vault.md | host-side MITM that swaps stub→real creds + fences egress | live; OAuth re-aimed at the broker model |
| Sessions / §0 | docs/session-event-log.md | durable event log, drive (`send`) + read (`subscribe`/`watch`) surface | local live; managed evidence copied into the local log `[verified 2026-08-31]` |
| Snapshots | docs/config.md, AGENTS.md | rustic repo, push/pull, bookmarks | local + R2 live `[verified 2026-07-16]` |
| Dispatch / eval | docs/dispatch.md, docs/eval.md | fork-k verified workers; the eval runner | local libkrun shipped `[verified 2026-07-16]` |
| Managed / CF tier | docs/managed-tier.md, docs/durable-object-usage.md | bounded D1/R2 execution + CF Sandbox placement | experimental foreground backend; no custom Pillbox DO `[verified 2026-08-31]` |

## Entanglements that bite

The cross-cutting dependencies that *look* separable but aren't. These are the
ones that produced wrong "authoritative" answers — check here before claiming a
change is clean.

### libkrun uses Docker for fresh image exports
`materialize_rootfs` exports an OCI image through Docker, then boots a private
clone of its completed seed. On macOS, fresh ordinary `rootfs/v5` generations and
bounded `repository-images-v2` generations share private case-sensitive APFS
backing with their runtime clones; see [the storage contract](libkrun-sandbox.md).
Docker is required for fresh exports and image-tag resolution. Ordinary launches
may reuse a complete current-generation seed when Docker is unavailable, after
validating its backing. This is image plumbing: `src/sandbox/oci.rs` is the one
file that runs `docker` (runner-image resolution, `docker create`/`export`/`rm`),
and pillbox has no Docker execution backend `[verified 2026-11-27]`.

### One substrate value per record, one constructor `[verified 2026-11-27]`
`SubstrateKind::selected()` (src/sandbox/substrate.rs) reads `PILLBOX_BACKEND`
once (unset/`libkrun`, `managed`; `docker` and anything else are usage errors),
and `SubstrateKind::backend()` is the **only** place a kind becomes a
`Box<dyn SandboxBackend>`. Session and sandbox records store a `Substrate`
(`Libkrun(Option<LibkrunHandle>)` / `Managed(ManagedHandle)` / `Retired`) as a
`[substrate]` TOML table; pre-substrate records (`backend` + `sandbox_id` /
`backend_ref`) migrate on read, and docker/e2b/undecodable ones load as
`Retired` — listable and removable, never driven. The JSON `backend` /
`placement` / `sandbox_id` keys are derived from the stored value.

### The backend seam `[verified 2026-11-27]`
One backend = one impl of two traits in `src/sandbox/mod.rs`:
`SandboxBackend` (`run` + `capabilities` + `kind` + `live` + `target`, plus
substrate-only operations with unsupported defaults: `spawn_sandbox`, `login`,
`score_in_sandbox`, `verify_project`, `execute_repository`, `host_checks`) and
`LiveSession` (`send`/`attach`/`spawn_log_tailer`/`http`/`workspace_path`/
`ingest`/`kill`, capability-gated via `Caps`). `live_session()` and
`sandbox_target()` match only on `substrate.kind()` and go through
`SubstrateKind::backend()`; nothing matches a backend-name string.
The build boundary is one feature: only `mod libkrun`, the libkrun arm of
`SubstrateKind::backend`, code that names `crate::sandbox::libkrun` (the
`execution::microvm` drivers, the hidden `text` command, the VMM re-exec
entrypoints) and libkrun-only tests are gated — 16 `cfg(... feature =
"libkrun")` lines in `src/`. The build without the feature is the in-guest
tools build: it has no substrate, so `SubstrateKind::Libkrun.backend()` is an
exit-2 usage error there. Its dead code is allowed once at the crate root and
linted on the full build (CI runs a type-check-only `cargo clippy` with default
features on Linux).
`run` returns a typed `Started` and prints nothing; `pillbox run`'s `--json`
envelope and banners are rendered by `commands::run::render_started`
(src/commands/run.rs) `[verified 2026-11-26]`.

### Session verbs are in-process calls; the CLI is one client `[verified 2026-11-26]`
`commands::run::start` (resolve agent + descriptor defaults, validate, `--memory`,
`backend.run`) and the session cores in `src/commands/session/` — `send`,
`stream::wait_idle`, `workspace_of`, `score`, `rehydrate_result` — return typed
values and print nothing to stdout; in-process readers of a session's §0 log use
`events::log::read_log`. The `pillbox run`/`session …` handlers render those
values; the `--json` shapes (`session::started_json`, `session_info`,
`score_verdict_json`) live only in that rendering, so in-process callers must not
depend on them. `Started::into_live_session` is the one place that decides which
run outcomes a caller can go on driving. Because `run::start` runs inside
long-lived callers, every detached child it spawns (`__krun-vmm`,
`__session-tailer`) is spawned as a `sandbox::DetachedChild`, whose `Drop` hands
it to a waiter thread, so an exited child never lingers as a zombie that
signal-0 liveness probes read as alive.

### libkrun launch shape `[verified 2026-06-20]`
`krun_create_ctx` → `set_vm_config` → `set_root(dir)` (virtio-fs root, *not* a
disk) → `add_virtiofs` shares → `add_net_unixstream` (egress; needs libkrun built
`NET=1`) → `add_vsock_port2` (control/attach) → `set_exec` → `start_enter` (boots,
never returns, forks a `__krun-vmm` subprocess). Kernel is bundled (libkrunfw).
The egress L2 frames go to a host socketpair where a **smoltcp** userspace stack +
the vault MITM live (src/sandbox/libkrun/egress.rs).

## Open structural debts

- **Daemonless OCI image pull** — replace `docker create/export` so libkrun
  doesn't need the docker daemon.
- **Spec/code drift** — several big docs (vnext and parts of the optimization
  family) mix design history with current behavior. They need curation into
  current canonical docs vs `docs/archive/`.
