---
id: vault-egress
paths:
  - "src/vault/**"
  - "src/sandbox/libkrun/vault.rs"
  - "src/sandbox/libkrun/egress.rs"
  - "src/sandbox/libkrun/mitm.rs"
  - "src/sandbox/libkrun/jit_refresh.rs"
  - "src/commands/vault.rs"
  - "src/secrets.rs"
  - "src/url_safety.rs"
entry:
  - "src/vault/refresh.rs:pre_refresh"
  - "src/vault/token_store.rs:TokenStore"
  - "src/sandbox/libkrun/mitm.rs"
tests:
  - "src/vault/token_store.rs:concurrent_callers_coalesce_to_one_forward"
  - "src/vault/token_store.rs:abort_intact_clears_pending_and_next_begin_retries"
  - "src/vault/egress.rs:default_deny_blocks_unmatched"
---

# Credential vault and outbound policy

The vault supplies guest-visible credential stubs and releases real access tokens at the host boundary for the matching provider. Host-side coordination owns OAuth refresh. Egress policy can deny unmatched destinations. See [vault](../vault.md) and [secrets](../secrets.md).

## Sub-features

- Provider-scoped OAuth/API-key substitution and token-endpoint refusal.
- Single-writer pre-refresh, pending-state recovery and libkrun JIT refresh.
- Host/IP policy, explicit allowlists, per-run CA and optional persistent CA.

## How to get to it (user POV)

Inspect `pillbox vault status`; run a configured vault-capable agent with `pillbox run --vault --egress-deny`. Add explicit destinations with `--egress-allow`. The guest uses stubs; the host broker handles refresh.

## Driving it with the repository harness

`cargo test --no-default-features vault::token_store::tests` exercises local credential fixtures, including concurrent refresh ownership and ambiguous failure recovery. `cargo test --no-default-features vault::egress::tests` checks swap/pass/deny and IP rejection outcomes. These tests do not contact an OAuth provider or prove live TLS routing.

## Gotchas

- An ambiguous refresh leaves a pending marker so another caller cannot reuse a possibly consumed token. A provably non-consuming failure can clear it.
- The maintained libkrun MITM has JIT refresh; the deprecated host proxy has different lifetime constraints. Do not extrapolate one path to the other.
- Vault substitution alone is not a blanket egress restriction; read the selected path’s policy and explicit deny setting.
