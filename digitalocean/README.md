# DigitalOcean managed compute

Status: experimental, disabled by default. A live DO adapter smoke on 2026-09-23
passed text/schema output, exact retry, status/cancel, and cleanup after fixing the
native structured-output fallback. The control plane used local SQLite/filesystem
ports: deployed Worker/D1/R2 and signed-grant/currentness integration remain
unverified. No production rollout is implied. Sanitized results live in Huddles'
`docs/evaluations/digitalocean-runtime-smoke-2026-09-23.md`.

Pillbox's existing Cloudflare-hosted authenticated execution/2 service can select
DigitalOcean Harness Runtime as compute. It continues to use D1 for bounded claims,
R2 for one immutable terminal artifact, and Huddles for authorization/collaboration.
The provider choice lives in the existing hashed/signed transport field:

| Field | DO value |
| --- | --- |
| `execution.transport.harness` | `opencode` |
| `execution.transport.transport` | `digitalocean-sandbox-exec` |
| `execution.transport.harness_version` | `1.18.31` |
| `execution.transport.adapter_revision` | `pillbox/digitalocean/1` |
| `execution.placement` | `managed_container` |
| `execution.requested.provider` | `openrouter` |
| `tool_policy` | `deny_all` |

Managed OpenRouter runs currently admit only `openai/gpt-5-mini` with normalized
`low`, `medium`, or `high` effort. The adapter installs an explicit OpenCode variant
with OpenRouter `reasoning.effort` and `provider.require_parameters: true`, then
selects that variant in the native request. Unsupported pairs (including Qwen3
Coder) fail before allocation. Huddles' optional profiles seal `high`.

The rest of execution/2 is unchanged. Old Cloudflare requests keep their hashes;
old deployments reject the new transport before runtime allocation. There is no
cross-provider fallback. Huddles' `openrouter-gpt5mini-digitalocean` profile chooses
GPT-5 mini; `openrouter-gpt5mini-cloudflare` chooses the same inference on Cloudflare.

## Supported boundary

Each invocation creates a fresh DO sandbox from one immutable, operator-owned
agent config, then runs a fresh native OpenCode session through its loopback HTTP
API. A bounded detached Python worker records the completed native response and
kills its OpenCode server. Pillbox polls the result, validates structured output,
records actual served-model and token evidence, and deletes the DO sandbox before
returning success. It never uses DO managed chat or DO inference. A completed
native `StructuredOutputError` may supply raw JSON to the trusted schema validator,
as on Cloudflare; other native errors remain failures.

Input is capped at 32 KiB UTF-8 plus 16 KiB output schema; the guest response is
capped at 96 KiB and provider HTTP bodies at 256 KiB. Readiness has at most 30 polls with a 60-second deadline;
turn collection has at most 135 polls and a 270-second deadline. Each API call has
a 30-second deadline; control exec requests have a 15-second guest timeout. The
existing ten-minute invocation lease and global execution allowance remain in force.
The guest also has an independent 260-second alarm. These are bounds, not an infrastructure spend cap.

Tools, workspace provisioning/finalization, imported native conversation,
local/cloud session handoff, arbitrary harness versions and managed Codex are not
supported by this adapter. A sealed prompt can contain authorized HCP context; it
cannot adopt a vendor conversation or a Cloudflare-provisioned workspace. The
workspace-oriented Rust `pillbox run --backend managed` path remains Cloudflare.
Do not point it at DO and assume files were transferred. Rustic-over-R2 remains
the workspace store; adding its DO transfer requires a separate tested capability.

## Configure an isolated environment

1. Apply `cloudflare-spike/migrations/0005_digitalocean_allocations.sql` after the
   existing four migrations. Keep the existing authorization bootstrap and allowance
   setup. Migration 0005 is used only by the DO path.
2. With a scoped DO token in the operator environment, create an immutable config:

   ```sh
   doctl --http-retry-max 0 harness-runtime config create \
     --spec digitalocean/agent.example.yaml --name pillbox-opencode-v1
   ```

   The manifest resolves `OPENROUTER_API_KEY` from the operator environment. Never
   commit its resolved form. The config captures the secret; per-invocation creates
   send only its ID. This shared config remains operator-owned and is not deleted
   with individual sessions. Validate its OpenCode image, Python availability,
   OpenRouter secret, deny policy, egress, and idle timeout before enabling it.
   A stock-image version change fails the guest version check before sampling.
3. Set the variables shown in `digitalocean/.env.example` on the isolated Worker.
   Store `DIGITALOCEAN_API_TOKEN` as a Worker secret. Its scope must permit session
   create/read/exec/delete; config creation belongs to the operator setup. The token
   never enters the sandbox. `DIGITALOCEAN_ALLOCATION_NAMESPACE` must be unique to
   this execution realm, and stable until its allocation records are retired.
4. Keep both switches at `0` until a reviewed allowance is seeded. Enable both
   `MANAGED_EXECUTION_ENABLED` and `DIGITALOCEAN_EXECUTION_ENABLED` only for the
   authorized test. Select the DO transport in a signed private invocation or an
   exact public execution capability; merely changing the Worker config does not
   rewrite any previously sealed invocation.
5. Observe one new invocation, exact retry, bounded status, cancellation, cleanup,
   and actual DO/OpenRouter billing. Disable DO admission after the smoke. Existing
   status/cancel operations remain available with the admission switch disabled;
   retain the DO token until cleanup completes.

No deployment, config creation, or paid inference is performed by the test suite.

## Recovery and operation budget

`digitalocean_allocations` retains at most one row per admitted invocation. It
contains only invocation/name/config/provider-session identity and a lifecycle
state. A cancellation tombstone can fence allocation before create. Reservation
precedes DO create; a create or launch with an ambiguous outcome is never repeated.
Cleanup finds a lost create by an exact namespace-derived name, rejects multiple
matches and config mismatch, and only retries deletion. There are no periodic
scans, background schedulers, or new Durable Objects.

Normal allocation work is one insert, one attach update, up to 136 indexed state
reads, one stop upsert, and one deleted update. These D1 observations enter the
existing cost meter. An authorized terminal/expired status read can reconcile
cleanup with one stop upsert, at most one bounded provider name lookup, one delete,
and one deleted update. Provider API requests and billed lifecycle dollars are
not measured by the v1 envelope. `sandbox_profile: digitalocean/configured` prevents
confusing the substrate with Cloudflare, and total cost remains unknown.

A `stopping` row with no provider ID is intentionally unresolved if a lookup finds
nothing: a timed-out create might still finish later. Repeat authorized status or
cancel after inspecting the provider. Do not mark it deleted from an immediate
empty list. If the provider definitively rejected create, an operator may reconcile
that row against retained API evidence. Do not clear a tombstone to rerun the same
invocation. Retire allocation records only with their execution/idempotency records,
after provider deletion is verified. The shared allowance bounds retained rows.

## Verification

```sh
cd cloudflare-spike
node --test digitalocean_runtime.test.ts digitalocean_guest.test.ts execution_service.test.ts
./node_modules/.bin/tsc --noEmit
```

API source: official DigitalOcean doctl v1.171.2 vendored
[godo hosted-agent client](https://github.com/digitalocean/doctl/blob/v1.171.2/vendor/github.com/digitalocean/godo/hosted_agents.go).
Native transport: [OpenCode server API](https://opencode.ai/docs/server/).

Reasoning controls were checked against [OpenRouter model metadata](https://openrouter.ai/api/v1/models),
[required parameter routing](https://openrouter.ai/docs/guides/routing/provider-selection), and
[pinned OpenCode variant mapping](https://github.com/anomalyco/opencode/blob/v1.18.31/packages/opencode/src/provider/transform.ts).
