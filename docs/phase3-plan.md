# Plan: substrate-plane Phase 3 (flip default + doctor + harden)

libkrun becomes the local default. Because it's then the near-sole local
substrate with no docker fallback, this phase lands the **hardening** and
**backend-aware doctor** alongside the flip, plus the caps consumers deferred
from Phase 2. Unit suites (487/517) stay green; the flip's live behavior is
verified by `scripts/smoke/run.sh` at the boundary (needs a healthy libkrun host).

Critical path: P3-000 → (P3-001 ‖ P3-002) → P3-003. P3-004 (docs) is independent.

## P3-000 — Shared libkrun host probes

A small set of pure probe fns, consumed by both `doctor` (P3-001) and the launch
preflight (P3-002), so the "is this host able to run a microVM" logic has one
home:
- `virtualization_available() -> Result<(), reason>` — `/dev/kvm` on Linux, HVF
  (`sysctl kern.hv_support`) on macOS.
- `runtime_deps_present() -> Result<(), reason>` — libkrun's undeclared dylibs
  (`libepoxy`, `molten-vk`) resolve (the `brew cleanup` footgun).
- `disk_headroom(path) -> u64` (bytes free, via `statvfs`) + a `MIN_HEADROOM`
  const — the disk-pressure stall guard.

```yaml
id: P3-000
task_type: feature
depends_on: []
footprint:
  creates: ["src/sandbox/libkrun/host.rs"]
  modifies: ["src/sandbox/libkrun/mod.rs::*"]   # one `mod host;` + re-export
gate: "cargo clippy --all-targets --features libkrun -- -D warnings clean; unit tests for disk_headroom (>0 on cwd) + the probes returning a typed reason; default build unaffected (module is cfg(libkrun))"
```

## P3-001 — Backend-aware `doctor`

`doctor` today only probes Docker (`doctor.rs:134-183`) — on a libkrun-default
host it wrongly fails "Docker not running." Make it report the active backend;
add libkrun checks (gated `#[cfg(feature="libkrun")]`) using P3-000: virtualization
present, runtime deps resolve, disk headroom, and an orphaned-`__krun-vmm` scan.
Demote Docker to an optional "compat backend present?" check (not a hard fail).

```yaml
id: P3-001
task_type: feature
depends_on: ["P3-000"]
footprint:
  modifies: ["src/doctor.rs::*"]
gate: "cargo clippy (default + --features libkrun) -D warnings clean; `pillbox doctor --json` on a libkrun build reports libkrun checks and does NOT hard-fail on absent Docker; existing doctor tests updated + green"
```

## P3-002 — Launch + teardown hardening

In `src/sandbox/libkrun/session.rs` (+ `mod.rs` where the VMM child is spawned):
- **Disk preflight** before `cow_clone_*` / `materialize_rootfs`: if `disk_headroom`
  < `MIN_HEADROOM`, fail loud with a "free space" `Next:` (no half-booted stall).
- **SIGABRT→deps mapping**: when the VMM child dies by `SIGABRT` at boot, map it to
  an actionable error naming the likely missing deps (`brew install libepoxy molten-vk`),
  instead of an opaque signal death.
- **Process-group reaping**: spawn the VMM child in its own process group
  (`pre_exec` `setsid`); `kill_session` kills the **group** (`killpg`), not just the
  pid; add a sweep that reaps orphaned `__krun-vmm` groups when the session list is empty.

```yaml
id: P3-002
task_type: feature
depends_on: ["P3-000"]
footprint:
  modifies:
    - "src/sandbox/libkrun/session.rs::*"
    - "src/sandbox/libkrun/mod.rs::vmm_child_main"
gate: "cargo clippy --features libkrun -D warnings clean; full libkrun suite (517) stays green; teardown still reaps the VMM child (existing kill_session/session-rm tests green); a disk-preflight unit test (forced low headroom → loud error)"
assumptions:
  - "process-group change must NOT break the existing detached-child reparent/reattach flow — the existing tests are the guard"
```

## P3-003 — Default flip + caps consumers

1. **Flip** `select_backend()` (`src/sandbox/mod.rs`): with `#[cfg(feature="libkrun")]`,
   default to libkrun unless `PILLBOX_BACKEND=docker`. Non-libkrun builds stay docker.
2. **Sandbox group** (`commands/sandbox.rs:118`): gate the docker hardcode on
   `caps().long_lived_exec` — explicitly use the docker backend for the long-lived
   exec sandbox (libkrun lacks it), with a clear error if docker is unavailable.
   This keeps `sandbox spawn` working after the default flip.
3. **detached_vault** (`docker.rs` run-path): the `--detach`+`--vault` rejection is
   docker-hardcoded today — express it via `caps().detached_vault` so libkrun (which
   supports it) isn't wrongly bound by docker's limitation. (Behavior unchanged on
   each backend; just sources the gate from caps — more caps consumers, addressing
   the "Caps is decorative" review note.)

```yaml
id: P3-003
task_type: feature
depends_on: ["P3-002"]
footprint:
  modifies:
    - "src/sandbox/mod.rs::select_backend"
    - "src/commands/sandbox.rs::*"
    - "src/sandbox/docker.rs::run"
gate: "cargo clippy (default + --features libkrun) -D warnings clean; full suites green (update any test asserting the default backend); `sandbox spawn` still uses docker; `PILLBOX_BACKEND=docker` forces docker on a libkrun build (unit-test the selection)"
```

## P3-004 — Recast docker as the compat backend (docs)

Update `CLAUDE.md` (the direction note), `README`, and any user-facing doctor/run
messaging: libkrun is the local default; docker is the no-KVM **compat** backend,
not the default. No code.

```yaml
id: P3-004
task_type: docs
depends_on: []
footprint:
  modifies: ["CLAUDE.md", "README.md"]
gate: "the direction note + command surface describe libkrun-as-default + docker-as-compat consistently; no stale 'docker is the default' claims remain"
```

---

**Validation:** 5 tasks; empty-`depends_on` = P3-000 + P3-004 = 40% ≤ 50% ✓.
Concurrent pair P3-001 ‖ P3-002 touch `doctor.rs` vs `libkrun/session.rs` (+ disjoint
`mod.rs` symbols: a re-export vs `vmm_child_main`) — run sequentially anyway to avoid
working-tree contention. Gates are clippy (both) + the full suites + targeted unit
tests; the **flip's live behavior is gated by `scripts/smoke/run.sh`** at the phase
boundary. **`/vuln-triage` consideration:** Phase 3 adds host-probe + reaping +
a default flip — no new external input/network/auth surface, so still defer the
security pass to Phase 4 (the vsock `send` input channel).
