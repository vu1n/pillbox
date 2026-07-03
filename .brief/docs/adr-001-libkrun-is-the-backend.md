---
id: adr-001-libkrun-is-the-backend
project: pillbox
type: decision
status: active
title: libkrun is THE local backend and the default build
supersedes: docs/decisions.md#ADR-001
related_code:
  - "src/sandbox/mod.rs"
  - "src/sandbox/libkrun/**"
  - "Cargo.toml"
---

<!-- brief:anchor libkrun-is-the-backend -->
## Target libkrun as the single local backend; `default = ["libkrun"]`

libkrun (local microVM; KVM on Linux, HVF on macOS) is the one backend pillbox
targets. `cargo build` with no flags produces a libkrun binary; all new work
targets libkrun. Docker is demoted to the container-family compat backend, not
the default (see `adr-002-docker-backend-deleted`).

**Why.** A solo maintainer cannot carry two backends at parity — the divergence
was a standing bug source (it stranded `sandbox spawn/exec`, `session send`, and
the 2b vault coordinator on docker, and produced the 2026-06-20 vault clobber).
libkrun is the runtime the maintainer actually runs, and its VM boundary makes
"safe to run a prompt-injected agent" real rather than aspirational. Rejected:
holding docker at "cheap parity" (never cheap); making the agent guess
per-feature which backend to use.

### Invariant
- `Cargo.toml` `[features]` sets `default = ["libkrun"]`; a bare `cargo build`
  yields a libkrun binary.
- New capabilities are wired for libkrun first; a docker-only feature that
  strands libkrun is a regression, not an acceptable state.
- CI's platform-agnostic job may build `--no-default-features` (toolchain-free);
  the dedicated libkrun job (macOS/brew) covers the libkrun toolchain.
