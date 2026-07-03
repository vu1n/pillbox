---
id: workspace-cow-fork
project: pillbox
type: decision
status: draft
title: Workspace fork = materialize-once base cache + copy-on-write clone
related_code:
  - "src/workspace/base_cache.rs"
  - "src/workspace/cow.rs"
---

<!-- brief:anchor workspace-cow-fork -->
## Forking a workspace = restore-once base cache + copy-on-write clone

A swarm forks `k` workers from one base workspace, and the fork must be near-free — it
"moves no per-run bytes" (the property asserted by
`doc://pillbox/workspace-ingest-sovereignty@latest#workspace-ingest-sovereignty`). This
decision is the *mechanism* behind that property, in two halves:

1. **Materialize-once base cache** (`base_cache.rs`) — `k` workers off one immutable
   snapshot pay a *single* restore, not `k`. `materialize_once` fills a shared,
   content-addressed cache entry exactly once; the restore itself is a caller closure, so
   the core carries no backend/storage dependency (the libkrun path wraps it with the
   rustic pull).
2. **Copy-on-write clone** (`cow.rs`) — each worker's fork is a CoW directory clone (APFS
   `clonefile` / Linux `FICLONE` reflink), so a fork moves no data blocks.

**Why.** A k-worker swarm pays *fork* cost repeatedly, not restore cost — so making the
fork a reflink (and the restore a one-time cache fill) is what keeps dispatch cheap. The
seam is deliberately backend- and OS-agnostic — one primitive for the libkrun/HVF backend
today and the intended QEMU/KVM one — so the fork mechanism is never locked to a VMM or an OS.

### Invariant
- The base-cache key MUST be a **stable content id** (e.g. a snapshot handle): the cache
  never invalidates, so a mutable key would serve stale data forever.
- A restore runs **exactly once** per key — `flock`-serialized, completion marker written
  last, and a partial/interrupted entry is rebuilt, never served truncated.
- A fork is an **independent** copy-on-write clone, not a shared-storage alias: writing one
  side never disturbs another.
- Where the filesystem can't reflink, the clone degrades to a **reported** byte copy
  (`CloneMethod::Copied`), never a silent one — a broken "free fork" promise stays visible.
- The primitive preserves mode (0600 creds) and symlinks, and is tied to no VMM or OS.

<!-- refines: workspace-ingest-sovereignty (the "moves no per-run bytes" property);
     wraps adr-006-rustic-cache-variant-gated (the rustic pull is the base-cache restore) -->
