---
id: adr-003-qemu-parked
project: pillbox
type: decision
status: active
title: QEMU evaluated and parked — keep the trait seam open
supersedes: docs/decisions.md#ADR-003
related_code:
  - "src/sandbox/mod.rs"
---

<!-- brief:anchor qemu-parked -->
## Do not build a QEMU backend now; keep the `SandboxBackend` seam open for it

No QEMU backend is built. The `SandboxBackend` trait seam stays open so one
could be added, but it is not a current target. Revisit **only** if a
no-hardware-virt target appears.

**Why.** libkrun is cross-platform too (the "macOS-only" reading was just
`clonefile` + a missing build flag), so QEMU's only differentiator is running
*without* hardware virt (TCG software emulation) — and TCG is too slow to
justify. Requiring KVM/HVF is an accepted cost. Rejected: QEMU as the single
backend now; libkrun + QEMU both (the two-backend tax again).

### Invariant
- No QEMU backend impl lives in the tree; adding one is a deliberate future
  choice gated on a no-hardware-virt requirement, not accreting speculative code.
