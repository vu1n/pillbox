---
id: libkrun-env-fork-substrate
project: pillbox
type: decision
status: active
title: The libkrun microVM owns egress in userspace; the guest never holds the real credential
related_code:
  - "src/sandbox/libkrun/egress.rs"
  - "src/sandbox/libkrun/mitm.rs"
  - "src/sandbox/libkrun/vault.rs"
  - "src/sandbox/libkrun/session.rs"
---

<!-- brief:anchor libkrun-env-fork-substrate -->
## Terminate the guest's network in a host-owned smoltcp stack; MITM-swap on egress

The libkrun backend gives the guest a real NIC (virtio-net) whose packets
terminate in a **host-owned userspace TCP stack** (smoltcp) running in the
`__krun-vmm` child, not in the parent. That single userspace termination point is
where the vault MITM (rustls), the default-deny egress fence, and telemetry live.
The guest mounts **stubs**; the real credential reaches the child out-of-band and
is swapped stub→real on the outbound request — it never enters the VM env/argv.

**Why.** Owning the termination point is the only place to gate, MITM, and measure
egress; libkrun's zero-config TSI gives no userspace control point (and on
macOS/HVF didn't even carry egress). Running the MITM in the VMM child (not the
parent) is what lets a detached, vaulted session outlive the launching CLI — the
child keeps the agent + egress stack + vault alive. A secret in the VM env would
be readable by the agent via `/proc/self/environ` and exfiltrable by a prompt
injection; the env-fork is the security thesis.

### Invariant
- The real credential value never lands in the guest env, argv, or VmSpec — only
  the child (a host process) sees it; the guest gets stubs.
- Egress terminates in a **per-sandbox** smoltcp stack (one stack + one pin table
  + one vault + one poll loop per microVM); never share one egress stack across
  trust domains.
- `krun_start_enter` does not return — the VMM child *becomes* the VM while the
  parent supervises; this subprocess split is the spine for attach + §0 + detach.

> **Built vs specced (backfill 2026-07-01):** the substrate is largely built and
> live-verified (L1–L7): boot, vsock control, attach + §0 over vsock, virtio-net
> + smoltcp egress with a DNS fence (NXDOMAIN default-deny at the name layer),
> owned rustls MITM + stub→real swap, CoW workspace + secret scrub, detached +
> vaulted sessions, opencode server-mode. **Deferred:** response-side real→stub
> for mid-run OAuth token *refresh* (a refresh response still rotates reals into
> the guest — see `adr-004-vault-broker-oauth`); IP-level (vs name-level) DNS pin
> for arbitrary-NAT; rootfs-cache GC (no GC today — known debt).
