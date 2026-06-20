# QEMU backend spike — EVALUATED, DEFERRED (2026-06-20)

**Decision: stay on libkrun. QEMU noted, not built.** Once it was clear libkrun
is cross-platform too (Mac/HVF + Linux/KVM — the "macOS-only" reading was just
`clonefile` + a missing `make BLK=1 NET=1` flag), the only thing QEMU adds is
running *without* hardware virt (TCG). And **QEMU in TCG (software emulation) is
too slow** to be worth it. libkrun stays THE backend; the `SandboxBackend` /
`LiveSession` seam stays open so QEMU (or docker, or another) can be added later
if a no-hardware-virt target ever justifies it. The rest of this doc is the
preserved analysis for that future revisit.

---

**Original question:** can QEMU be pillbox's *single cross-platform* microVM
backend (Mac/HVF + Linux/KVM + CI/TCG)? Decided on evidence, not vibes.

Reference: `earendil-works/gondolin` is a working QEMU agent-sandbox with the
same host-side userspace-net + MITM + secret-injection model — proof the shape
works, and a template for the parts pillbox hasn't built.

## What's reusable (most of it)

- The `SandboxBackend` / `LiveSession` / `Caps` trait seam — QEMU is one more impl.
- The guest **rootfs directory** (`~/.pillbox/krun/rootfs/<img>`), extracted from
  the runner image. QEMU consumes it as an `mkfs.ext4` disk or via virtiofsd.
- The **egress stack**: smoltcp userspace TCP/IP + the vault MITM + credential
  swap (`src/sandbox/libkrun/egress.rs`). It eats raw L2 ethernet frames — which
  is exactly what QEMU `-netdev socket`/`dgram` emits. Only the frame-transport
  shim changes (libkrun passt header → QEMU socket framing).
- The §0 plumbing, creds/workspace shares, vault, snapshots — backend-agnostic.

## The deltas (what the spike builds)

| Concern | libkrun | QEMU |
|---|---|---|
| Root | `krun_set_root(dir)` (virtio-fs) | `-kernel vmlinuz` + ext4 disk **or** virtiofsd root |
| Kernel | bundled (libkrunfw) | explicit `vmlinuz` (alpine `linux-virt` / gondolin) |
| L2 net | `krun_add_net_unixstream` (passt) | `-netdev socket/dgram` + `virtio-net` |
| Control | `krun_add_vsock_port2` | `vhost-vsock` (`-device vhost-vsock-pci`) |
| Exec | `krun_set_exec` | init in rootfs / kernel cmdline |
| Accel | HVF (mac only) | HVF (mac) / KVM (linux) / TCG (CI, no virt) |

## Phases

1. **Boot probe** — `qemu-system-aarch64 -machine virt,accel=hvf` boots the
   pillbox rootfs (ext4 from the krun rootfs dir) + an acquired `vmlinuz`, runs
   one command, prints it. Measure cold boot wall-time. *Decides: does QEMU+HVF
   boot our guest, and is boot latency acceptable (vs libkrun ~125ms; only
   matters if it's seconds, not hundreds of ms).*
2. **Net + vault** — wire `-netdev` to the existing smoltcp/MITM stack (frame
   shim), run one vaulted agent turn, confirm the credential swap works.
3. **CI green via TCG** — the same probe on `ubuntu-latest` with `accel=tcg`
   (no KVM needed). *Decides: cheap cross-platform CI — the thing libkrun can't do.*

## Decision

- All three pass + boot adequate → **QEMU is the single backend.** libkrun
  demoted to optional fast-path or dropped; CI returns to cheap Linux; the
  macOS-only constraint and the libkrun Linux-portability debt both vanish.
- Boot unacceptably slow for fan-out → keep libkrun as the mac fast path, QEMU
  as the Linux/CI backend (accept two backends, but *only* if speed demands it).

Throwaway code is fine — this answers the question, it isn't the backend.
