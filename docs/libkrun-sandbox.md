# libkrun sandbox — the substrate pivot (Docker → microVM)

**Status:** direction / spec (not yet built). Supersedes the Docker-daemon local
backend and the whole remote-backend line ([remotes-redesign.md](./archive/remotes-redesign.md),
[remotes.md](./archive/remotes.md) — now deprecated).

## Why

Docker was chosen for one reason: **VPS ubiquity for the remote plane.** That
reason is gone — "remote" resolved to *Cloudflare-managed* or *pillbox-running-
locally-on-the-VPS* (see [vnext.md](./vnext.md)), neither of which needs a
local Docker daemon driven over SSH. With the remote rationale removed, the
local runtime is free to be the *best* one instead of the most *compatible* one.

[libkrun](https://github.com/containers/libkrun) is that runtime:

- **Secure** — the isolation boundary is a **VM** (KVM on Linux, Hypervisor.
  framework on macOS), not a shared kernel. This turns the pitch's first word
  from aspirational to real: safe to run a prompt-injected agent. (Today's
  threat model explicitly does *not* defend against container escape.)
- **Fast / small** — sub-100ms boot, tiny footprint, **no daemon** (a linked
  library + a spawned helper, not `dockerd` / Docker Desktop). No-daemon finally
  *matches* pillbox's identity instead of fighting it.
- **macOS-native** — HVF microVMs directly, no Docker Desktop (which is itself a
  hidden Linux VM running containers; libkrun is the leaner direct path).

**We own it, we don't fork it.** libkrun is an LGPL *library*; we FFI its C API
(`include/libkrun.h`) + depend on the `libkrunfw` kernel artifact — the same way
[brood-box](https://github.com/stacklok/brood-box),
[microsandbox](https://github.com/superradcompany/microsandbox), and
[krunai](https://github.com/slp/krunai) all link it. We are a *sibling* of those
projects, not a fork of any. The sandbox is table-stakes; **the layer above —
drive-from-chat + great telemetry — is what's ours**, and owning the substrate
is what keeps that layer's channel clean (see [§ Channels](#channels)).

## Architecture

```
host: pillbox  ──FFI──▶ libkrun (KVM/HVF microVM)
   │                       │
   │  vsock (control)      ├─ virtio-fs ← workspace (COW snapshot)
   │  ◀───────────────────▶│  pillbox-init (PID 1): runs the agent,
   │  frames + §0 events    │              speaks the control channel
   │                       │
   └─ smoltcp egress stack ◀─ virtio-net ← agent's internet
      (vault v2 + egress firewall + telemetry)
```

- **`pillbox-init`** — a small Rust binary, PID 1 in the guest. Boots, execs the
  agent (or `opencode serve`), and exposes the control channel. The natural home
  for the in-guest half of the frame protocol + the §0 event producer (the role
  `bbox-init` plays for brood-box, but ours, in Rust, over vsock).
- **Rootfs** — an OCI image works as the microVM rootfs (krunvm/crun-krun
  style), so the existing runner-image artifact survives the pivot; a slimmer
  custom rootfs is an option later.
- **Workspace** — host dir shared via **virtio-fs**; a **COW snapshot**
  (FICLONE / `clonefile(2)`) gives near-instant per-run isolation and is the
  clean local "fork N agents from one base" primitive. rustic stays the
  *durable / cross-machine* store; COW is the *fast local fork*. They compose.

### Channels

Two **separate** channels — do not conflate:

- **Control (`pillbox-init` ↔ host): frame protocol + §0 events → vsock.**
  virtio-vsock is the purpose-built host↔guest pipe, independent of the guest's
  internet. *Open question:* vsock-on-HVF via libkrun may be finicky — both
  macOS references (brood-box, krunai) used SSH-over-the-guest-network instead.
  Prototype vsock first; fall back to a forwarded localhost socket if needed.
  `pillbox-init` doesn't care which.
- **Egress (the agent's internet) → a userspace TCP stack (smoltcp).** libkrun
  offers TSI (zero-config socket impersonation, simplest boot, little control)
  vs **virtio-net + a host-side userspace stack** — which is where microsandbox
  uses **[smoltcp](https://github.com/smoltcp-rs/smoltcp)** to *terminate the
  guest's connections in userspace*. That termination point is exactly where the
  **vault, the egress firewall, and network telemetry** live, so owning it
  serves all three priorities. Boot on TSI; move egress to smoltcp when wiring
  vault/egress/telemetry (i.e. early).

**The convergence:** the two differentiators — *drive from Slack/Discord/chat*
and *great telemetry* — ride the **same** `pillbox-init` channel: `send` →
control → agent (drive); agent events → control → host → §0 log / OTLP
(telemetry). One owned channel, both jobs. That is the concrete reason to own
the substrate rather than rent a sandbox SDK.

## Security model (union of the two references)

Adopt by axis; the two references are strong on different ones:

- **Network / credentials → microsandbox's model** (vault v2): default-deny
  egress allowlist + **credential substitution only on a verified TLS handshake
  to an allowlisted host** (the real key is swapped in only when the connection
  provably goes to the right place; the agent never sees it). A direct upgrade to
  today's blind stub-swap, living in the smoltcp egress stack.
- **Filesystem / workspace → brood-box's model**: COW snapshot + **non-
  negotiable secret-file exclusion** (`.env*`, `*.pem`, `.ssh/`, `.aws/` — an
  untrusted repo's config cannot negate it) + multi-layer policy where an
  untrusted repo **cannot widen** egress or loosen a security setting.
- **Egress profiles** (steal from brood-box): `permissive` / `standard` /
  `locked` — great UX, orthogonal to the review gate.
- **Diff-review-before-flush is OPTIONAL.** brood-box's blocking human gate
  before every flush fights pillbox's driven/chat/autonomous priority.
  *Interactive* mode may offer it; *driven* mode uses COW + snapshot-and-pull.

## What survives the pivot (most of the recent work)

The §0 / attach / opencode work is **transport-agnostic at its core**, so it
ports by swapping only the bottom:

| Layer | Survives unchanged | Changes |
|---|---|---|
| §0 log, event mapper, `drain_sse`, synth | ✅ (consume a `Read`/payloads) | — |
| frame protocol, `session send/watch/subscribe`, pump | ✅ surface | transport: `docker exec` → vsock |
| opencode integration | ✅ the `message.*` mapper (the brain) | bridge: `docker exec curl` → vsock / guest-net |
| vault | ✅ concept | sidecar-in-container → smoltcp egress proxy (vault v2) |
| sandbox backend | — | `local_docker` + `docker::` → a `libkrun` backend |

## Superseded / deprecated by this pivot

- **Local Docker backend** (`sandbox/local_docker.rs`, `docker.rs`) → a libkrun
  backend. Code currently ships; deprecated in direction.
- **Remote backends** — `docker://`, `ssh://`, `e2b://` (`remote_docker`,
  `remote_ssh`, `remote_e2b`) and [remotes-redesign.md](./archive/remotes-redesign.md).
  "Remote" is now Cloudflare-managed or pillbox-local-on-the-box; the SSH-driven-
  daemon model is retired. Code currently ships; deprecated in direction.
- The Docker **runner image** framing in [runner-image.md](./runner-image.md) →
  microVM rootfs (OCI still usable).

## Proven recipe — macOS boot (2026-06-01) ✅

Step 1 done: a Linux 6.12.76 microVM boots on macOS 26 (Apple Silicon, HVF) via
Rust→libkrun FFI and runs a command in an Alpine rootfs. The working recipe
(proof crate at `~/code/libkrun-boot`, kept out of this repo until it graduates
into a `pillbox-krun` backend):

- **Install:** `brew install slp/krun/libkrun` (bottled; pulls `libkrunfw`).
- **FFI:** hand-written `extern "C"` — no bindgen needed for the minimal surface:
  `krun_create_ctx` → `krun_set_vm_config(ctx, vcpus, ram_mib)` →
  `krun_set_root(ctx, rootfs_dir)` → `krun_set_workdir` →
  `krun_set_exec(ctx, path, argv_after_0, envp)` → `krun_start_enter` (never
  returns; guest stdout streams to the host; process exits with the guest's status).
- **Rootfs:** a plain directory works as the virtio-fs root — an extracted OCI/
  Alpine `minirootfs` tarball is enough (OCI-image-as-rootfs confirmed viable).
- **macOS gotcha (the time-sink):** libkrun `dlopen`s a *bare* `libkrunfw.5.dylib`
  via `libloading`. macOS 26 does **not** resolve it via `DYLD_LIBRARY_PATH`
  (stripped/ignored even for linker-adhoc binaries) nor the `$HOME/lib` fallback.
  It resolves against the **main executable's `LC_RPATH`** — so the consumer
  binary needs `-Wl,-rpath,/opt/homebrew/lib` (this is exactly how `krunai`
  works). Build it in `build.rs`.
- **HVF entitlement:** the binary must be codesigned with
  `com.apple.security.hypervisor`:
  `codesign -f --entitlements ent.plist -s - <binary>` (ad-hoc is fine; `cargo`
  re-signs the binary each build, so re-sign after every `cargo build`).
- **vsock-on-HVF: WORKS** (step 2, below) — no SSH fallback needed.

### Control channel (step 2, proven) ✅

`pillbox-init` (the guest workload) → host frame round-trip over vsock, on HVF:

- **Host:** `krun_add_vsock_port(ctx, PORT, "/tmp/pillbox-ctrl.sock")` — default
  direction is *guest connects out, host listens*. The host `UnixListener::bind`s
  that path **before** `krun_start_enter`; libkrun connects to it when the guest
  dials the vsock port. (`krun_add_vsock_port2(..., listen=true)` flips it: guest
  listens, host initiates — use that later if the host should attach on demand.)
- **Guest:** `socket(AF_VSOCK)` → `connect({ cid: VMADDR_CID_HOST=2, port: PORT })`
  → write the length-prefixed frame. Retry the connect (~5s) since the host
  listener races boot.
- **Guest binary:** `pillbox-init`, cross-compiled to
  `aarch64-unknown-linux-musl` (static, `libc` for AF_VSOCK) with
  `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld` — no external gcc;
  copied into the rootfs and set as `krun_set_exec` target.
- **Note:** the exec'd binary is the guest *workload*, not literally PID 1 —
  libkrun runs its own internal init and spawns our process as a child. So
  `pillbox-init` doesn't need full-init duties; it just owns the agent + the
  control channel and parks while serving.

## Build order (proof-first)

1. ✅ **Boot proof** — done. FFI = hand-written; rootfs = OCI/Alpine dir;
   macOS = rpath + hypervisor entitlement.
2. ✅ **`pillbox-init` + control channel** — done. vsock works on HVF; a frame
   flows guest→host over `krun_add_vsock_port`'s unix-socket bridge.
3. **Attach port** — frame protocol / `session attach` over the control channel.
4. **§0 producer** — events over the control channel → durable log (watch/subscribe).
5. **Egress + vault v2** — smoltcp stack: TLS-verified credential substitution +
   default-deny egress + profiles + network telemetry.
6. **Workspace** — COW snapshot + non-negotiable secret-file exclusion.
7. **opencode** — repoint the bridge transport to the control channel.
8. **Deprecate Docker** — remove the docker/remote backends once libkrun is at parity.

## References

- [brood-box](https://github.com/stacklok/brood-box) — the security model blueprint.
- [microsandbox](https://github.com/superradcompany/microsandbox) — SDK shape + the credential-proxy / smoltcp egress model.
- [krunai](https://github.com/slp/krunai) — minimal libkrun mechanics (boot, virtio-fs, gvproxy/passt, SSH).
