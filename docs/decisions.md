# Decisions (ADR log)

The load-bearing decisions, with status. **This is the anti-drift record:** if you
think "didn't we decide X?", it's here. Changing a decision means adding a new
dated entry that supersedes the old one — never a quiet reversal in conversation
or a contradicting edit elsewhere. Each entry says what was **rejected**, too, so
the rejected option doesn't get re-proposed.

Format: `STATUS` · what · why · what it means concretely · what's rejected.

---

## ADR-001 — libkrun is THE backend (and the default build)
**Status: Accepted (2026-06-20). PR #102, #103 merged.**

- **Decision:** libkrun (local microVM; KVM on Linux, HVF on macOS) is the one
  backend pillbox targets. `default = ["libkrun"]` — `cargo build` (no flags)
  produces a libkrun binary.
- **Why:** a solo maintainer cannot carry two backends at parity; the divergence
  was a standing bug source (it stranded `sandbox spawn/exec`, `session send`,
  and the 2b vault coordinator on docker, and produced the 2026-06-20 vault
  clobber). libkrun is what the maintainer actually runs.
- **Concretely:** new work targets libkrun only. CI's platform-agnostic `test`
  job builds `--no-default-features` (the toolchain-free path) so it needs no
  libkrun toolchain; the dedicated `libkrun` job (macOS/brew) covers libkrun.
- **Rejected:** holding docker at "cheap parity" (never cheap); making the agent
  guess per-feature which backend to use.

## ADR-002 — Docker: backend DELETED, image/build plumbing KEPT
**Status: Accepted (2026-06-20). Backend deletion not yet executed.**

- **Decision:** delete the docker **backend** (`DockerBackend` /
  `DockerLiveSession` — the run-the-agent-in-a-container path). **Keep** docker as
  an image/build tool (it pulls + unpacks the runner image into libkrun's rootfs
  — see [architecture.md](./architecture.md)); that part gets renamed
  `crate::oci` to make the boundary honest. Re-adding a backend later =
  implement the `SandboxBackend`/`LiveSession` trait + a `docker-backend-archive`
  git tag as reference — not code kept in the tree.
- **Why:** the backend is the divergence/parity source. "Leveraging docker for
  build is ok" (maintainer, 2026-06-20) — the image tool is fine; the second
  backend is not.
- **Rejected (do not re-propose):** "deprecate but keep the docker backend in the
  tree" — that's the trap that lets it re-accrete and re-strand features. The
  maintainer rejected this explicitly, more than once.
- **Open consequence:** docker-the-daemon stays a *runtime* dependency for
  libkrun's images + `auth login` until two follow-ups land (daemonless OCI pull;
  auth-in-libkrun). Tracked in [architecture.md](./architecture.md).

## ADR-003 — QEMU evaluated, parked
**Status: Accepted (2026-06-20). See docs/qemu-spike.md.**

- **Decision:** do not build a QEMU backend now. Keep the trait seam open for it.
- **Why:** libkrun is cross-platform too (the "macOS-only" reading was just
  `clonefile` + a missing `make BLK=1 NET=1` flag), so QEMU's only differentiator
  is running *without* hardware virt (TCG software emulation) — and **TCG is too
  slow** to justify. libkrun requiring KVM/HVF is accepted.
- **Revisit only if:** a no-hardware-virt target appears. Reference then:
  `earendil-works/gondolin` (its `host/src/qemu/` proves pillbox's smoltcp+vault
  model ports to QEMU).
- **Rejected:** QEMU as the single backend now; libkrun + QEMU both (the
  two-backend tax again).

## ADR-004 — Vault OAuth uses the broker model, not in-proxy refresh
**Status: Accepted (2026-06-20). Not yet built. PR #101 (in-proxy coordinator) parked.**

- **Decision:** the sandbox gets a **dummy** creds file with a far-future expiry +
  the vault MITM **injects** the real `Authorization: Bearer`; a host-side
  **pre-refresh** owns token rotation. The agent never refreshes.
- **Why:** dissolves *both* the host-creds clobber *and* the refresh-token-reuse
  coordination problem at once — the agent-never-refreshes move makes the broker
  the sole refresher by construction. Validated independently by centaur +
  gondolin (both inject host-side; the sandbox sees only a placeholder).
- **Context — the clobber:** root-caused 2026-06-20: the in-proxy coordinator is
  *correct* (it forwards direct to Anthropic and commits a real token); the
  clobber is a docker-on-macOS bind-mount interaction, docker-backend-only,
  libkrun unaffected. So the coordinator wasn't the bug — the whole in-proxy
  approach is just more fragile than the broker.
- **Rejected:** the in-proxy refresh coordinator (PR #101) as the path; letting
  the agent refresh through a coordinated proxy and capturing the rotation.

## ADR-005 — Docs are a router + canonical-per-subsystem + this log
**Status: Accepted (2026-06-20).**

- **Decision:** AGENTS.md is a slim **router** (mental model + subsystem map +
  links here and to architecture.md + the working discipline). Each live
  subsystem has ONE canonical doc marked with a STATUS/verified-date header. The
  CLI reference moves to docs/commands.md. Planning/research/superseded docs move
  to docs/archive/ and are NOT authoritative.
- **Why:** the failure mode is an agent answering authoritatively on partial or
  stale context. 37 sprawling docs with no current-vs-aspirational signal made
  retrieval unreliable, so the agent guessed. The fix is curation + a trust
  contract, not more prose.
- **The discipline (binding):** read a subsystem's canonical doc before acting on
  or claiming things about it; **code wins over doc** — if they disagree, fix the
  doc in the same change.
