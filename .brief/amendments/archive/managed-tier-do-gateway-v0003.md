# Amendment proposal: managed-tier-do-gateway

**Decision:** doc://pillbox/managed-tier-do-gateway@0002#managed-tier-do-gateway
**Proposed by:** Codex, at the maintainer's request
**Date:** 2026-09-23
**Status:** proposed; requires human ratification before implementation

## What should change

Allow DigitalOcean Harness Runtime as an additional managed execution substrate.
Cloudflare remains the default. The managed control plane can remain on Cloudflare;
selecting DO compute does not migrate claims, evidence, or collaboration state.

Replace the opening phrase “addresses an isolated Cloudflare Sandbox” with
“addresses an isolated sandbox on an explicitly selected, supported managed
provider (initially Cloudflare Sandbox or DigitalOcean Harness Runtime).”

Replace the first invariant with:

> The managed backend provisions nothing on the host; managed execution is a
> network call through Pillbox's authenticated execution service into an explicitly
> selected supported provider. Cloudflare is the backward-compatible default.
> Provider selection is bound before provisioning to immutable execution identity,
> request hashing, authorization, and recovery. An ambiguous result or provider
> failure never silently switches providers or samples another turn.

Generalize “The bucket-wide parent key never crosses to Cloudflare” to “The
bucket-wide parent key never crosses to any execution provider.” Preserve the
fresh prefix-scoped transfer and session-token requirements.

Add these provider invariants:

- Inference provider and execution provider are separate selections. DO execution
  may use OpenRouter. Requested configuration never substitutes for served-model
  evidence.
- D1 retains bounded managed invocation, owner, allowance, and lifecycle claims;
  R2 retains immutable evidence. Rustic-over-R2 remains the authoritative workspace
  store and shared local/cloud snapshot handle space. Vendor disks/checkpoints
  are derivative and must not become the only recoverable workspace copy.
- DO is a managed substrate adapter, not a restored SSH/e2b/docker remote backend,
  Huddles orchestrator, or new Pillbox-authored Durable Object.
- New providers must preserve exact-retry behavior, deny-all tool admission,
  bounded execution/evidence, owner-qualified status/cancel, capability currentness,
  cancellation, and kill-before-transfer. Unsupported capabilities fail closed
  before provider provisioning or model sampling.
- Provider authentication stays in the trusted adapter. Provider credentials are
  never included in sealed HCP packets, durable execution evidence, or model input.
  Only the scoped credentials required by the existing guest boundary are passed.
- Runtime allocation identity and cleanup progress must survive an ambiguous API
  response. Provisioning is never blindly retried. Cleanup is bounded, observable,
  and independently retryable without model sampling; missing billing observations
  remain unknown. DO gets an explicit admission switch and reviewed allowance.
- Native conversation continuation is a separate versioned checkpoint capability,
  not an implication of workspace snapshot support. Until sealed-packet lineage,
  checkpoint integrity/version checks, and single-writer handoff are implemented,
  unsupported local/cloud continuation requests fail closed. Importing a native
  session must not bypass Huddles context visibility or immutable packet binding.

All other clauses remain unchanged, including Huddles collaboration ownership,
no per-delta database persistence, default-deny Durable Objects, and the existing
separately ratified exception required for a custom Durable Object.

## Why

The maintainer requested DO as another cloud runtime after a funded sample.
Native OpenCode export/import successfully resumed one session local → DO → fresh
local, and returned synthetic uncommitted workspace state. Direct cloud execution
used OpenRouter. Managed chat did not automatically adopt the imported native
session. Resumed CLI processes saved completed inference but failed to exit before
timeouts; this must be addressed rather than hidden with another model attempt.

The existing decision explicitly requires Cloudflare placement. Adding a DO
adapter contradicts that clause even when all ownership boundaries are preserved.
The evidence supports an optional provider, not replacement or production enablement.

Evidence in the sibling Huddles repository:
`docs/evaluations/digitalocean-portability-2026-09-23.md` and its JSON companion.
The sample did not test R2 handoff, cancellation, tool-policy enforcement, or
production recovery. Approval of this amendment does not claim those gates passed.

## Code that needs it

Implementation after ratification, contracts before adapters:

1. Define provider selection, immutable binding, compatibility for existing
   Cloudflare requests, and provider-specific usage in Pillbox's
   `cloudflare-spike/src/codex_execution.ts`, `run_cost.ts`, Rust
   `src/sandbox/managed/execution_contract.rs`, and synchronized Huddles
   `app/contracts/src/execution.ts`. Version any wire change that existing strict
   decoders cannot accept; preserve historical hashes and replay.
2. Implement DO behind the existing `ExecutionRuntime` boundary in
   `cloudflare-spike/src/execution_service.ts`; wire selection in
   `huddles_runtime.ts` and managed provisioning/finalization. Keep shared D1/R2
   claim and evidence ownership; do not fork the execution service. Verify the
   authenticated DO API and a bounded OpenCode transport before choosing the
   adapter protocol. The eval's timeout-prone CLI is not production-ready.
3. Extend the Rust managed client/config so explicit DO selection reaches the
   same authenticated service. Add credential/config documentation and examples
   without placing real credentials in the repo.
4. Update Huddles typed routing/profile selection, canonical request/authorization
   fixtures, and execution attribution for the selected provider. Preserve realm
   qualification and effect-backed adapter boundaries. Retain the Cloudflare
   service binding where it still addresses Pillbox's control plane.
5. Test provider binding/hash conflicts, unsupported capability rejection before
   side effects, ambiguous provisioning, exact retry without second sampling,
   cancel/status ownership, secret redaction, bounded cleanup, and provider-specific
   cost missingness. Exercise rustic R2 restore/finalize and fault recovery before
   advertising workspace handoff. Treat native session handoff as a separately
   tested capability with sealed context lineage.
6. Run cross-repo schema parity, targeted Rust/TypeScript suites, topology policy,
   Huddles architecture checks, and Brief conformance. Enable live DO smoke only
   under an explicit small allowance, cleanup deadline, and retained evidence.

## Impact / risk

No deployment, production secret changes, or additional funded smoke is authorized
by this proposal itself. The scope is optional DO runtime support and its tests;
Cloudflare remains the default and local libkrun behavior remains unchanged.

DO stock images must satisfy exact harness/version and tool-policy contracts;
model availability alone is insufficient. Adding a provider expands credential,
cleanup, and cost-accounting surfaces. Tests must prove those boundaries before
advertising support. No automatic cross-provider fallback is permitted, and R2
workspace portability must not be confused with live process migration.


---
ratified_rev: 0003
ratified_by: maintainer
