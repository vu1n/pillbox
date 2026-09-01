---
id: adr-002-docker-backend-deleted
project: pillbox
type: decision
status: active
title: Docker — the agent-run backend is demoted, deletion pending
supersedes: docs/decisions.md#ADR-002
related_code:
  - "src/sandbox/mod.rs"
  - "src/sandbox/docker.rs"
---

<!-- brief:anchor docker-backend-deleted -->
## Delete the docker *backend* once libkrun is at parity; keep image/build

The docker **backend** (the run-the-agent-in-a-container path) is slated for
deletion once libkrun reaches parity. **Docker as an image/build tool is kept**
— it pulls and unpacks the runner image into libkrun's rootfs; that boundary
should be made honest (the build-tool role, not a second agent-run backend).
Re-adding an agent-run backend later means re-implementing the trait plus a
git-tag archive for reference — not code kept resident in the tree.

> **Drift (backfill 2026-07-01):** the deletion is **not yet executed**.
> `src/sandbox/docker.rs` still ships a full `DockerBackend` +
> `DockerLiveSession` implementing `run()` and the live-session verbs, and there
> is **no `crate::oci` module** yet — the build-tool boundary rename hasn't
> happened. The decision (delete once at parity) still holds; the code is at the
> "demoted, present" stage, not the end state.

**Why.** The second agent-run backend is the divergence/parity source that
stranded features and produced the vault clobber (see
`adr-001-libkrun-is-the-backend`). "Leveraging docker for build is ok" — the
image tool is fine; the second backend is not. Rejected (do not re-propose):
"deprecate but keep the docker backend in the tree" — that is the trap that lets
it re-accrete and re-strand features; rejected explicitly, more than once.

### Invariant
- Docker code that remains exists to **build/materialize the runner image**, not
  to run an agent as a competing backend.
- Docker-the-daemon may stay a *runtime* dependency for image pull + `auth login`
  until daemonless OCI pull and auth-in-libkrun land — a tracked open
  consequence, not a permanent second backend.
