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
**Status: Accepted (2026-06-20). BUILT on BOTH vault paths — host-side/docker #107, libkrun #109. JIT (PR-B) deferred. PR #101 parked.**

- **Decision:** the sandbox gets a creds file with a **far-future expiry** + the
  vault MITM **injects** the real `Authorization: Bearer`; a host-side, single-writer
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
- **The core is backend-agnostic.** `vault::refresh::pre_refresh` + the `TokenStore`
  single-writer + `abort_intact` + the far-future sentinel
  (`STUB_FAR_FUTURE_EXPIRES_AT_MS`) operate on a creds-file *path* — nothing
  backend-specific. Each backend just *wires* them in.
- **Built #107 (host-side proxy = docker path):** `provision` stamps the stub's
  `expiresAt`; `provision_oauth_mount` routes the start-of-run pre-refresh through
  the core. NOTE: `src/vault/`'s `VaultSession` is constructed **only by
  `docker.rs`**, so #107 alone did NOT reach libkrun — a wrong-backend miss caught
  on review.
- **Built #109 (libkrun = the real backend):** libkrun has its own vault
  (`stub_claude_oauth` + the in-VMM byte-swap MITM, no `VaultSession`).
  `stub_claude_oauth` post-dates the stub expiry; `prepare_launch` calls
  `pre_refresh` on the live creds file before the CoW clone, fail-closed. Same core.
- **Deferred (PR-B):** JIT-refresh-at-the-MITM. On the host-side proxy that's a
  per-request async handler; on libkrun the MITM is a static byte-swap in the
  `__krun-vmm` child (a *host* process, so it can call the same coordinated
  `pre_refresh` itself — but needs an expiry-aware egress-loop refresh state
  machine, not a port). Closes the >token-lifetime (~8h) session case and lets the
  in-proxy/byte-swap 401-fallback be deleted. Until then a long session relies on
  that (uncoordinated) fallback.
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

## ADR-006 — Rustic cache is variant-gated (off for Local, on for S3)
**Status: Accepted (2026-06-20).**

- **Decision:** `RusticBackend::repo_opts` sets the rustic cache per variant:
  `no_cache(true)` for `RusticVariant::Local`, cache **on** for
  `RusticVariant::S3` with `cache_dir` anchored at `<state-dir>/cache`. (Before
  this, all three open paths hard-coded `no_cache(true)`.)
- **Why:** the blanket `no_cache(true)` was a local-first default from the first
  rustic landing (PR #10), never differentiated when the S3 variant arrived. For
  S3 it's a perf footgun — every host-side open (push/pull/list) re-fetches the
  index from the bucket. rustic's cache is content-addressed + immutable (keyed by
  config id), so there is **no** staleness/correctness reason to disable it, and
  it's safe under concurrent swarm access.
- **Concretely:** Local stays cache-off *on purpose* — caching a local-disk repo
  into a second local dir is pure write amplification for zero latency gain, so
  do **not** "fix" it back to a blanket setting. S3 caches under the per-pillbox
  state dir (scoped + cleanable), not rustic's global XDG dir. scrypt key
  derivation (~5s/open) is unaffected — the cache never caches the key, only
  index/pack data; the per-open scrypt cost is a separate matter.
- **Rejected:** blanket `no_cache(true)` for all variants (the footgun);
  enabling the cache in rustic's global XDG dir (unscoped, not cleanable with the
  pillbox state dir).

## ADR-007 — Runner image tags name roles, not history
**Status: Accepted (2026-06-22). PR #118.**

- **Decision:** the runner image has exactly three tag *roles* — `dev` (moving:
  built locally by `scripts/build-runner.sh`, published by CI on merge-to-main),
  `latest` (moving: CI on stable release, alias of the newest `vX.Y.Z`, the
  built-in `DEFAULT_RUNNER_IMAGE`), and `vX.Y.Z` (immutable, per release). A
  `pillbox.toml` pins `dev` or `latest`; you pin a concrete `vX.Y.Z` **only** when
  you need a reproducible run (e.g. a frozen eval/σ̂ baseline).
- **Why:** the old `l5`/`l6`/`l7`/`l8` "generation" tags smuggled version control
  into Docker tag names. They leaked into `pillbox.toml`, ~20 scripts, CI, and
  memory, so "which tag is current?" became a research task — and the tag churned
  live (a config flipped `l7`↔`l8` between reads on 2026-06-22). Docker tags are
  pointers, not a DAG; roles are stable, generation numbers are not.
- **Concretely:** all scripts + the `new -i` wizard prefill + CI default to
  `dev`; `DEFAULT_RUNNER_IMAGE` stays `…:latest`. Reproducibility lives in
  immutable `vX.Y.Z` tags, never in a moving tag. Historical "Live-verified
  (`pillbox-runner:l7`)" notes in docs keep their number — they record *which
  libkrun dev phase* was checked, a changelog, not a current-tag pointer.
- **Rejected:** per-generation tags (`l<N>`) or ad-hoc names (`rolling`, one-off
  spikes) as the thing configs point at; freezing a *moving* tag as a baseline
  (use `vX.Y.Z`). NB: the local `dev` image was never byte-reproducible while its
  agents were unpinned — pin agents (the `runner/Dockerfile` ARGs) **and** a
  `vX.Y.Z` tag for a real baseline.
