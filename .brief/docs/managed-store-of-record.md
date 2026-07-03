---
id: managed-store-of-record
project: pillbox
type: decision
status: active
title: Rustic-over-R2 is the managed store of record; CF snapshots are a fork mechanism only
related_code:
  - "src/sandbox/managed.rs"
  - "cloudflare-spike/src/session_gateway.ts"
---

<!-- brief:anchor managed-store-of-record -->
## Own the store, rent the fork: rustic-over-R2 is authoritative; CF backup/restore is derivative

The managed tier's **store of record** is pillbox's own rustic-over-R2 repo — the same
client-encrypted, content-addressed, prune-permanent store the local backend uses, so a
snapshot handle means the same thing local and managed (one handle space). Cloudflare's
native snapshot primitive (`createBackup`/`restoreBackup`, and equally CF Artifacts) MAY be
used only as a **transient fork mechanism** derived from a rustic snapshot — never as the
durable store, never the sole copy of workspace state.

This is the remote half of `doc://pillbox/workspace-cow-fork@latest#workspace-cow-fork`:
the local plane forks via `materialize_once` + reflink; the managed plane forks via a
one-time "rustic snapshot → CF backup" per handle, then `k` sandboxes `restoreBackup` it
(CF mounts the squashfs read-only + a FUSE-overlayfs upper — a copy-on-write fork). Same
shape (`materialize_once` keyed by a content id), CF's API as the remote CoW layer.

**Why.** The criterion is daily **dev ergonomics for personal-first use**, not defensibility —
pillbox is an artifact you live in, not a product with a moat to hold. On that axis rustic
still wins the store, for practical reasons: it *already* backs both planes — the local
libkrun backend and the managed R2 transfer (`managed.rs`) — so keeping it as the one store
is **less** work than switching, and adopting a CF-native store would mean either a
two-engine bridge or a **fragmented handle space** (a local rustic handle ≠ a managed CF
UUID / git SHA), which is exactly what makes `collect` / lineage / `dispatch` awkward across
planes. Rustic's **permanence** (snapshots live until pruned) beats CF backups' 3-day TTL +
ephemeral mounts for a checkpoint you return to next week, and its **dedup** beats a full
squashfs archive per snapshot. Client-side encryption is a bonus rustic gives for free —
kept, not the reason, not a blocker. The fork/overlay *mechanism*, by contrast, is
commoditized and verified (CF `createBackup` = `mksquashfs` → R2, `restoreBackup` =
squashfs-lower + fuse-overlayfs-upper CoW) — so rent it rather than hand-roll it. Net: own
the store because it's already there and nicer to live with (one handle space, permanence,
dedup); rent the fork because CF ships it. Consistent with
`doc://pillbox/managed-tier-do-gateway@latest#managed-tier-do-gateway` (build above the
commoditized substrate).

### Invariant
- The recoverable source of truth for a managed workspace is the pillbox **rustic-over-R2**
  repo: a workspace's durable state MUST be reconstructable from a rustic snapshot handle
  alone, so local and managed share **one handle space**.
- CF-native fork/mount mechanisms (`createBackup`/`restoreBackup`, ArtifactFS, `mountBucket`)
  MAY be used freely wherever they improve ergonomics — as a fork layer or a fast mount
  **over** the rustic source of truth — but never as the sole/authoritative copy. A CF
  backup's TTL/ephemerality is fine *because* it is derivative.
- The rustic→CF-fork bridge, when built, is **materialize-once** keyed by the rustic
  snapshot handle (remote analog of `base_cache::materialize_once`): one rustic→squashfs
  backup per handle, then `k` sandboxes restore it — never `k` rustic restores. It is a
  **deferred optimization** — until managed k-worker dispatch exists and the per-session
  restore cost actually bites, the plain per-session rustic restore is fine; don't pre-build it.
- Client-side encryption + prune-permanence come free with rustic; keep them, but they are
  ergonomics bonuses, not hard constraints that may veto a better-ergonomics choice.

<!-- refines: managed-tier-do-gateway (the workspace-transfer R2 credential is this store's transfer path);
     mirrors: workspace-cow-fork (the local materialize-once + CoW fork this ports to the managed plane);
     wraps: adr-006-rustic-cache-variant-gated (rustic is the store both planes pull from);
     supersedes-in-brief: docs/managed-tier.md §"Considered: Cloudflare Artifacts" (document-don't-adopt) -->

<!-- RATIFIED 2026-07-03 (maintainer). The decision criterion that settled it is DEV ERGONOMICS
     FOR PERSONAL-FIRST USE, not moat/defensibility — pillbox is an artifact, and it compromises
     for better dev ergonomics. On that criterion the recommendation HELD, but was
     re-grounded: rustic wins because it already backs both planes and gives one handle space
     + permanence + dedup (nicer daily ergonomics), NOT because portability is a moat. So the
     "invert to a CF-native store" branch is CLOSED — switching would be more work +
     handle-space fragmentation, not less. (This resolves the earlier competitive-positioning
     vs managed-tier "moat" tension: neither framing drives it; ergonomics does.)

     One residual to verify before relying on the fork-mechanism half: CF multi-sandbox
     restore (createBackup in sandbox A, restoreBackup the handle in B and C, each getting an
     independent CoW overlay). Architecturally implied (R2-stored, UUID-addressed, download-by-id)
     but NOT explicitly documented — a one-shot spike closes it.

     Verified basis (2026-07-03, CF docs):
       developers.cloudflare.com/sandbox/api/backups  (createBackup=squashfs→R2; restore=CoW overlay; TTL 3d; ephemeral mount)
       developers.cloudflare.com/sandbox/api/storage   (mountBucket readOnly/prefix/credentialProxy)
       developers.cloudflare.com/sandbox/concepts/containers  (rootless; no privileged/kernel-modules; sandboxes isolated)
       cloudflare/artifact-fs + blog.cloudflare.com/artifacts-git-for-agents-beta  (ArtifactFS: git blobless lazy-mount; private beta) -->
