---
id: adr-004-vault-broker-oauth
project: pillbox
type: decision
status: active
title: Vault OAuth uses the broker model — the agent never refreshes
supersedes: docs/decisions.md#ADR-004
related_code:
  - "src/vault/refresh.rs"
  - "src/vault/session.rs"
  - "src/vault/providers/anthropic.rs"
  - "src/vault/providers/codex.rs"
---

<!-- brief:anchor vault-broker-oauth -->
## The broker refreshes host-side; the sandbox gets a far-future stub

The sandbox receives a creds file with a **far-future expiry** and the vault
MITM **injects** the real `Authorization: Bearer`; a host-side, single-writer
pre-refresh owns token rotation. The agent never refreshes. This is the broker
model, chosen over an in-proxy refresh coordinator (PR #101, parked).

**Why.** It dissolves the host-creds clobber *and* the refresh-token-reuse
coordination problem at once: making the agent-never-refresh move makes the
broker the sole refresher by construction, so two clients can't both rotate the
same refresh token and trip provider reuse-detection (which revokes the whole
token family). The in-proxy coordinator was *correct* but strictly more fragile.
The core (`vault::refresh::pre_refresh` + the far-future sentinel) is
backend-agnostic — it operates on a creds-file path; each backend just wires it.

### Invariant
- The guest's creds carry a far-future `expiresAt` sentinel
  (`STUB_FAR_FUTURE_EXPIRES_AT_MS`, `src/vault/refresh.rs`); the real token value
  is never written into the guest — the MITM swaps stub → real on egress.
- Exactly one writer rotates a given refresh token; a refresh token is POSTed to
  the provider **at most once** — an ambiguous outcome fails closed to re-auth,
  never a retry that could re-send a consumed token. The single writer is the
  `TokenStore` (flock + pending marker + `rotation_generation`,
  `src/vault/token_store.rs`).
- No path lets the agent refresh through a coordinated proxy and capture the
  rotation.

> **Drift (backfill 2026-07-01) — the built state is partial.** The broker
> *pre-refresh* path (`refresh.rs::pre_refresh` → `TokenStore`) and the
> far-future stub are built and correct. But two of the design's enforcement
> requirements are **not yet met** in code:
> 1. The in-proxy `/oauth/token` handlers still read the refresh token from the
>    per-run `server.registry` start-of-run snapshot
>    (`providers/anthropic.rs`, `providers/codex.rs`), **not** re-reading from
>    disk under the `TokenStore` lock — so two concurrent runs can each forward a
>    stale `RT0`. The "rotate *through* the TokenStore" (M1a) requirement is open.
> 2. `dispatch` does **not** force `--vault` for OAuth agents
>    (`src/commands/dispatch.rs`), so the enforcement table is aspirational.
> `docker --detach --vault` *is* correctly refused (`src/docker.rs`).
