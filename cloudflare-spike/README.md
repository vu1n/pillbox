# Pillbox managed execution spike

This directory contains the experimental Cloudflare managed runtime. It is a
single-controller execution service, not a multiplayer gateway.

## Topology

- Cloudflare `Sandbox` Durable Object: vendor-owned container lifecycle only
- D1 `execution`: bounded invocation claims and terminal references
- R2 `EXECUTION_EVIDENCE`: one immutable terminal artifact per invocation
- Analytics Engine `RUN_COSTS`: at most one compact point per terminal run
- Worker/service-binding routes: execute, status, cancel, workspace provision,
  and workspace finalize

There is no Pillbox-authored Durable Object class, Agents SDK, per-event SQLite
log, WebSocket replay stream, driver lease, or participant roster.

Managed Huddles calls carry an Ed25519 `huddles.execution-grant/1` over the
private `PillboxAuthorizationControlPlane` service binding. Pillbox validates
the independent request binding, recomputes execution/output hashes, verifies
the signature, and rechecks currentness before every ensure and invoke—even
when D1 can reuse a terminal result. Currentness v2 pins both signer key ID and
the SHA-256 fingerprint of the raw Ed25519 public key. Credential bindings fail
closed until a bounded, non-Durable-Object broker exists.

Public HTTP routes require short-lived HMAC capabilities bound to one operation
and exact session/invocation id (`MANAGED_CAPABILITY_SECRET`). Huddles uses the
same-account service binding instead. Public execution is `deny_all`: managed
runtime tools stay disabled until credentials can be brokered without placing
provider or workspace secrets in a prompt-controlled process.

## Checks

```sh
npm test
npm run check:contract
npx tsc --noEmit
node --test do_usage_policy.test.mjs
npx wrangler deploy -c wrangler.container.toml --dry-run --containers-rollout=none
```

The production deployment and retirement of any historical Durable Object
namespace are separate, explicit release actions. See
[`../docs/managed-tier.md`](../docs/managed-tier.md) and
[`../docs/durable-object-usage.md`](../docs/durable-object-usage.md).
