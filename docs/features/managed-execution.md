---
id: managed-execution
paths:
  - "src/sandbox/managed.rs"
  - "src/sandbox/managed/**"
  - "cloudflare-spike/src/**"
  - "cloudflare-spike/migrations/**"
  - "cloudflare-spike/wrangler*.toml"
  - "digitalocean/**"
entry:
  - "src/sandbox/managed.rs:ManagedBackend"
  - "cloudflare-spike/src/execution_service.ts:ExecutionService"
  - "cloudflare-spike/src/execution_store.ts"
tests:
  - "src/sandbox/managed/execution_client.rs:managed_turn_lock_rejects_same_session_without_cross_session_contention"
  - "src/sandbox/managed/execution_client.rs:managed_turn_lock_releases_while_an_inherited_descriptor_stays_open"
  - "cloudflare-spike/execution_service.test.ts:exact retry does not resample"
  - "cloudflare-spike/execution_service.test.ts:authorization failures precede every persistence access"
  - "cloudflare-spike/managed_public_route.test.mjs"
  - "cloudflare-spike/workspace_finalize.test.ts"
  - "scripts/smoke/cf.sh"
---

# Experimental managed execution

The experimental managed placement executes bounded OpenCode work using Cloudflare Sandbox or an optional constrained DigitalOcean adapter. D1 owns invocation claims, R2 owns immutable evidence, and the CLI imports returned evidence into its local log. See [managed tier](../managed-tier.md) and [DigitalOcean scope](../../digitalocean/README.md).

## Sub-features

- Capability-bound public authorization (HMAC capability bound to operation, exact request bytes and session/invocation id), `public_controller` owner identity, `deny_all` tool policy and allowance reservation.
- Exact-request execution, bounded status/cancel and terminal evidence/costs.
- Cloudflare workspace provision/finalize and optional DigitalOcean fresh deny-all turns.

## How to get to it (user POV)

A configured controller submits a sealed invocation to `POST /v2/executions` with a short-lived capability, then uses the capability-authenticated status/cancel operations. Workspace-capable Cloudflare execution first provisions its snapshot. The local CLI’s managed placement is foreground-only; provider selection does not grant DigitalOcean workspace handoff.

## Driving it with the repository harness

From the repository root, run `cargo test --no-default-features managed_turn_lock`.
These tests check same-session exclusion, cross-session independence and immediate
owner-drop release even when a fork-equivalent duplicate descriptor remains open.

From `cloudflare-spike/`, run `npm ci`, `npm test`, `npx tsc --noEmit` and `python3 check-contract-parity.py`. Tests exercise exact retries, authorization before persistence, the public route over wrangler `unstable_dev` and workspace-finalize failures. Then `../scripts/smoke/cf.sh` starts local workerd, checks capability crypto and expects HTTP 401 for an unauthenticated execution request. That smoke starts no paid agent/container; no managed burn-in harness exists.

## Gotchas

- Managed turn guards explicitly unlock on drop: closing only the owner's descriptor leaves the lock held while a concurrent fork retains its copy, even with close-on-exec.
- Managed execution is currently OpenCode-only and public execution is deny-all (`tool_policy` other than `deny_all` is 400 `unsupported_policy`); unsupported agents must fail before charged work.
- No custom Pillbox Durable Object, remote event log or collaboration/replay broker is implemented. Collaboration belongs to the calling controller.
- The optional DigitalOcean adapter does not imply automatic cross-provider retry. Exact identity and no-resampling still apply.
- No burn-in harness exists; a preview deployment needs a new Pillbox-recorded burn-in, and passing local tests does not authorize deployment.
