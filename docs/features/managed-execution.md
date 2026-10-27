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
  - "cloudflare-spike/execution_service.test.ts:exact retry does not resample"
  - "cloudflare-spike/execution_service.test.ts:authorization failures precede every persistence access"
  - "cloudflare-spike/workspace_finalize.test.ts"
  - "scripts/smoke/cf.sh"
---

# Experimental managed execution

The experimental managed placement executes bounded OpenCode work using Cloudflare Sandbox or an optional constrained DigitalOcean adapter. D1 owns invocation claims, R2 owns immutable evidence, and the CLI imports returned evidence into its local log. See [managed tier](../managed-tier.md) and [DigitalOcean scope](../../digitalocean/README.md).

## Sub-features

- Operation-bound authorization, owner identity and allowance reservation.
- Exact-request execution, bounded status/cancel and terminal evidence/costs.
- Cloudflare workspace provision/finalize and optional DigitalOcean fresh deny-all turns.

## How to get to it (user POV)

A configured controller submits a sealed invocation to `POST /v2/executions`, then uses the authenticated status/cancel operations. Workspace-capable Cloudflare execution first provisions its snapshot. The local CLI’s managed placement is foreground-only; provider selection does not grant DigitalOcean workspace handoff.

## Driving it with the repository harness

From `cloudflare-spike/`, run `npm ci`, `npm test`, `npx tsc --noEmit` and `python3 check-contract-parity.py`. Tests exercise exact retries, authorization before persistence and workspace-finalize failures. Then `../scripts/smoke/cf.sh` starts local workerd, checks capability crypto and expects HTTP 401 for an unauthenticated execution request. That smoke starts no paid agent/container and is not a managed burn-in.

## Gotchas

- Managed execution is currently OpenCode-only and public execution is deny-all; unsupported agents must fail before charged work.
- No custom Pillbox Durable Object, remote event log or collaboration/replay broker is implemented. Huddles owns collaboration.
- The optional DigitalOcean adapter does not imply automatic cross-provider retry. Exact identity and no-resampling still apply.
- Release/burn-in and observed cost receipts are separate evidence gates; passing local tests does not authorize deployment.
