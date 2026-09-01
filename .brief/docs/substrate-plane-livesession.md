---
id: substrate-plane-livesession
project: pillbox
type: decision
status: active
title: The live session is the polymorphic plane — one LiveSession + Caps, not string-match dispatch
related_code:
  - "src/sandbox/mod.rs"
  - "src/commands/session/**"
  - "src/sandbox/http.rs"
---

<!-- brief:anchor substrate-plane-livesession -->
## Dispatch the live-session control surface through `LiveSession` + `Caps`

The whole live-session control surface (`send`, `attach`, kill, live tailing,
the server HTTP handle, in-sandbox grade, ingest) is a polymorphic `LiveSession`
trait built by one factory (`live_session(&Session)`), with explicit **capability
negotiation** via a `Caps` struct — not scattered string-matches on
`Backend::parse`. `SandboxBackend` is launch + capability profile (`run`,
`capabilities`, `id`); the *session* is the plane every command calls, backend-blind.

**Why.** New capabilities used to accrete as docker-first match arms with libkrun
arms that `bail!("docker only")`; that silent drift is why libkrun+PTY couldn't
`send`/live-`watch`. Making the session the polymorphic thing, with capabilities
*declared* rather than chased, keeps the asymmetries honest (KVM-only real egress
fence stays uniquely libkrun; the plane *exposes* it via `Caps`, it doesn't chase
parity) and makes a future managed/CF backend a one-handler add.

### Invariant
- `LiveSession` / `Caps` must compile in a docker-only build (`--no-default-features`);
  only the libkrun impl is `#[cfg(feature = "libkrun")]`.
- Commands branch on `caps()`, never on a re-parsed backend string, for
  cap-gated verbs (`score_in_sandbox`, `ingest`, server `http`).
- Adding a backend = one new `LiveSession` impl + a `select_backend`/`live_session`
  arm, not new match arms across the command layer.

> **Backfill note (2026-07-01):** the source doc (`docs/substrate-plane.md`) is
> stamped "No code yet / draft plan (2026-06-17)", but the contract has since
> **shipped** — `LiveSession`, the 8-field `Caps`, and the 3-method
> `SandboxBackend` are live in `src/sandbox/mod.rs`. This decision is re-grounded
> to the built state; the plan doc is the historical design.
