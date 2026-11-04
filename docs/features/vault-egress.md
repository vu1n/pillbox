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
  - "src/sandbox/libkrun/mod.rs:egress_refusals_lists_each_refused_name_once"
  - "src/sandbox/libkrun/mod.rs:egress_refusals_never_echo_terminal_control_bytes"
  - "src/sandbox/libkrun/egress.rs:a_guest_cannot_grow_the_fence_log_past_its_cap"
  - "src/sandbox/libkrun/mitm.rs:a_handshake_refused_at_the_cert_gate_reaches_the_refusal_summary"
  - "src/sandbox/libkrun/session.rs:egress_refusal_summary_names_this_runs_refusals_and_skips_a_shared_log"
---

# Credential vault and outbound policy

The vault supplies guest-visible credential stubs and releases real access tokens at the host boundary for the matching provider. Host-side coordination owns OAuth refresh. Egress policy can deny unmatched destinations. See [vault](../vault.md) and [secrets](../secrets.md).

## Sub-features

- Provider-scoped OAuth/API-key substitution and token-endpoint refusal.
- Single-writer pre-refresh, pending-state recovery and libkrun JIT refresh.
- Host/IP policy, explicit allowlists, per-run CA and optional persistent CA.
- Refusal record: the libkrun fence logs every NXDOMAIN and SNI denial to a per-run file under `~/.pillbox/krun/egress/` (or `PILLBOX_KRUN_EGRESS_LOG`), capped at 4 MiB a run, including SNIs the allowlist gate refused a cert, and a foreground run (attached or structured) names the hosts it refused when it ends, cut to hostname characters. A shared `PILLBOX_KRUN_EGRESS_LOG` gets no per-run summary, since concurrent runs mix their lines there.

## How to get to it (user POV)

Inspect `pillbox vault status`; run a configured vault-capable agent with `pillbox run --vault --egress-deny`. Add explicit destinations with `--egress-allow`. The guest uses stubs; the host broker handles refresh.

## Driving it with the repository harness

`cargo test --no-default-features vault::token_store::tests` exercises local credential fixtures, including concurrent refresh ownership and ambiguous failure recovery. `cargo test --no-default-features vault::egress::tests` checks swap/pass/deny and IP rejection outcomes. These tests do not contact an OAuth provider or prove live TLS routing.

## Gotchas

- An ambiguous refresh leaves a pending marker so another caller cannot reuse a possibly consumed token. A provably non-consuming failure can clear it.
- The maintained libkrun MITM has JIT refresh; the deprecated host proxy has different lifetime constraints. Do not extrapolate one path to the other.
- Vault substitution alone is not a blanket egress restriction; read the selected path’s policy and explicit deny setting.
