---
id: managed-tier-do-gateway
project: pillbox
type: decision
status: active
title: The managed tier is our §0 gateway on Cloudflare — the Durable Object IS the sequencer
related_code:
  - "src/sandbox/managed.rs"
  - "src/sandbox/mod.rs"
  - "src/events/source.rs"
  - "cloudflare-spike/**"
---

<!-- brief:anchor managed-tier-do-gateway -->
## Build above the commoditized substrate: a Session Durable Object is the §0 gateway

The managed tier runs pillbox's §0/gateway layer on Cloudflare's placement, not a
placement competing with theirs. A per-session **Durable Object** is the gateway:
single-writer, co-located SQLite log, `seq` authority, actor stamping, roster,
attach fan-out — the DO *is* the sequencer the §0 spec defines. A CF Container
runs the agent (the same runner image); the container submits §0 events with
`seq=0` and the DO assigns. Managed joins `docker`/`libkrun` behind the same
`SandboxBackend`/`LiveSession` trait; the CLI surface is **placement, not a URL
to a daemon** (`PILLBOX_BACKEND=managed` + managed config env).

**Why.** The substrate (sandbox + container + credential proxy) is commoditized
(Cloudflare, Claude Managed Agents, smolvm); the daylight is the sequenced +
attributed + multiplayer-attach §0 log. Reimplementing the substrate is how we
lose; the DO-as-gateway is CF's own recommended one-DO-per-coordination-unit
pattern, so "start the managed tier" and "land the §0 multiplayer keystone" are
one move on the substrate where the keystone first earns its keep.

### Invariant
- The managed backend provisions **nothing on the host**; the durable session
  lives server-side in the DO — every verb is a network call.
- Workspace transfer hands the DO a **prefix-scoped, fresh-per-transfer** R2
  credential; the bucket-wide parent key never crosses to CF. The DO's S3 client
  must forward the scoped credential's `session_token` as `X-Amz-Security-Token`
  (frozen-contract requirement), and scoping is fail-closed.
- R2 blobs / DO logs must never contain raw OAuth tokens or unredacted provider
  auth responses.

> **Drift (backfill 2026-07-01) — built ahead of its doc status.** The source
> doc (`docs/managed-tier.md`) is stamped "design/proposed", but the host-side
> `ManagedBackend` + `ManagedLiveSession` are **shipped** (`src/sandbox/managed.rs`,
> ~66KB), wired into `select_backend()`, with the R2 prefix-scope mint and
> `/provision`+`/finalize` implemented; the `cloudflare-spike/` DO does the §0
> plane end-to-end (seq authority, actor attestation, driver arbitration,
> contract-parity-gated). **Open:** only the *foreground* run path is implemented
> (no detached managed finalize yet); real user token/secret provisioning; the
> consume-path opencode mapper is a second (TS) implementation and a drift surface.
