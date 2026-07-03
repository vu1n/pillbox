---
id: remote-backend-plane-removed
project: pillbox
type: decision
status: active
supersedes: docs/archive/remotes.md, docs/archive/remotes-redesign.md
title: The remote-backend plane (ssh/e2b/docker:// URLs) is removed; pillbox is local-only
related_code:
  - "src/sandbox/mod.rs"
  - "src/sandboxes.rs"
---

<!-- brief:anchor remote-backend-plane-removed -->
## No URL-transport remote backends; "remote" returns only as the managed placement

The SSH-driven / e2b / `docker://` URL remote backends and the `--remote` /
`remote add` surface are **removed** from the codebase. pillbox is local-only
(libkrun default, docker fallback). "Remote" returns not as a transport to
someone else's daemon but as a **managed placement** behind the same
`SandboxBackend` trait (see `managed-tier-do-gateway`).

**Why.** Running an agent on a remote daemon over SSH cost six prerequisite
setups before anything executed — "one architectural choice leaking out six
papercuts." The remote rationale that justified Docker (VPS ubiquity) dissolved:
"remote" resolved to Cloudflare-managed or pillbox-running-locally-on-the-box,
neither of which needs a local daemon driven over SSH. This decision records the
*current reality* and supersedes the two archived remote design docs
(`docs/archive/remotes.md` — shipped v0.6 behavior, now removed;
`docs/archive/remotes-redesign.md` — the Docker-context collapse, retired).

### Invariant
- No `RemoteSshSandbox`/`RemoteE2bSandbox`/`RemoteDockerSandbox` types, no
  `ssh://`/`e2b://`/`docker://` URL parsing, no `--remote`/`remote add` command in
  `src/`.
- Remote execution is only ever the managed placement behind the trait, never a
  resurrected URL-transport backend.
- The archived remotes docs are historical reference; the *carried-forward*
  reasoning (workspace-as-unit fork-from-store, the snapshot-lifecycle state
  machine) informs the libkrun CoW workspace, not a remote transport.
