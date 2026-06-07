# Cloudflare Durable-Object-as-§0-gateway — spike

A runnable `wrangler dev` spike proving the one load-bearing claim of the managed
tier ([docs/managed-tier.md](../docs/managed-tier.md)): **a per-session Durable
Object is the §0 sequencer + Subscribe fan-out, 1:1 with pillbox's local
`SessionLog`.** Everything else (sandbox/container, vault/broker, actor-auth) is
stubbed so the spike stays small.

> **✅ Validated live (2026-06-07)** on `wrangler dev` (workerd + real DO SQLite):
> `npm install && npx wrangler dev`, then `node smoke.mjs …` → append assigns
> monotonic seq 1-3, a `from=1` subscriber replays 1-3 then tails 4, in order, no
> gap/dup. Two fixes were needed and are in-tree: the Agents SDK requires the
> `nodejs_compat` flag (it imports node built-ins), and it **multiplexes its own
> `cf_agent_*` control frames onto the WS** — so a §0 subscriber must filter to
> Event envelopes (`seq` + `payload`); a real consumer (lum/Slack/browser) does
> the same. The lock-free seq (single-threaded-per-id + `transactionSync`) holds
> as designed.

## Why this is the right first slice

This is **milestone 0 minus the container hop** — it isolates the §0-over-DO
mechanics (seq persistence, hibernation replay+tail, reconnect-from-seq) before
paying for the container. The keystone finding from the research: a DO is
**single-threaded per id** and `ctx.storage.transactionSync` serializes appends,
so **seq monotonicity is lock-free** — `MAX(seq)+1` inside a sync transaction,
no distributed ordering, no lock primitive. The spec's feared "total-order under
partition" doesn't arise; the DO is the sole writer.

## Built on the Cloudflare Agents SDK (not raw DurableObject)

`SessionGateway extends Agent<Env>` (`agents` SDK, GA Apr 2026), routed via
`routeAgentRequest`. **This is the "use Cloudflare to its best" decision**, backed
by the CF Agents docs + the SDK design, and corroborated by external prior art:
GSV (a third-party gateway checked out at `~/code/gsv` — *not* ours) runs this
exact pattern — `Kernel`/`Process extends Agent`, custom `this.ctx.storage.sql`
stores, `connection.setState` per-connection, and crucially **never the global
`this.setState`**. The Agent class is a DurableObject subclass, so it's a strict
superset of the raw spike: it gives the hibernatable WebSocket lifecycle
(`onConnect`/`onMessage`), per-connection state, `getConnections()`/broadcast,
and `this.schedule()` (the container idle-TTL primitive) for free, while the
keystone `transactionSync` + `MAX(seq)+1` append survives **verbatim** (it's a DO
primitive the Agent inherits).

**The one conflict, declined:** global `this.setState` is last-write-wins,
whole-object, auto-synced — structurally incompatible with an ordered append-only
log keyed by monotonic `seq` (it'd lose ordering, replay, and per-event `actor`).
So the §0 log lives in `this.ctx.storage.sql` (our table, no sync) and the
subscriber cursor lives in `connection.setState` (per-connection, no global sync);
`this.setState` is **never called**. The Agents SDK helps everywhere and fights
us only on the one convention we opt out of.

## What's IN vs OUT

**IN** — a per-session Agent (instance name = `sessionId`) with SQLite `log`,
reached via `routeAgentRequest` at `/agents/session-gateway/<sessionId>/*`:
- `POST …/event` → append + assign monotonic `seq` (storage-backed, eviction-safe) → `{seq, head}`
- `GET …/subscribe?from=N` (WS upgrade → `onConnect`) → replay (`seq >= N`) then live tail, one Event/frame in seq order (hibernation-aware: per-connection cursor on `connection.setState`)
- `POST …/input` → append an attributed `input` Event (same path → also fans out)

**Sandbox wiring (cycle 1 — the DO↔container hop):** `/input` now drives a
per-session container — `getSandbox(env.Sandbox, sessionId).exec(cmd)` (with a
cold-start retry), and the result is appended as a §0 `tool_call` event, so a
subscriber sees the round-trip `input` (seq N) → `tool_call` output (seq N+1).
The Sandbox SDK is a sibling container-owning DO (`[[containers]]` + a Dockerfile
`FROM cloudflare/sandbox`), not this Agent. **Validated to the hop boundary:** the
worker compiles + runs with the Agents + Sandbox SDKs together, the cross-boundary
`exec` call fires, and its result/error round-trips into §0 and fans out (the
subscriber saw `input`→`tool_call` in order, carrying the real payload from the
Sandbox DO).

> **⚠️ Local container execution blocked on Apple Silicon (arm64).** wrangler dev
> builds the container for CF's prod platform (linux/**amd64**; the
> `cloudflare/sandbox` base is amd64-only — no arm64 manifest), and wrangler's
> local container runtime fails to boot that amd64 image on an arm64 host
> (`Container failed to start`) — even though plain `docker run` emulates it fine.
> So the §0 round-trip + the call path are proven, but the *container executes the
> command* leg needs amd64: a deploy, or a Rosetta-capable local runtime. A real
> CF-Containers-on-Apple-Silicon friction, not a wiring defect.

**OUT (stubbed, marked `// TODO`)**: the streaming-agent producer (in-container tailer POSTing `/event` back with `seq=0` — cycle 1 uses one-shot `exec`), the GSV-cribbed hibernate-safe pending-op routing table (cycle 2, gated on the container executing locally), vault/broker, `actor` authentication (body, not connection — milestone 1 stamps it in `onConnect`), driver-token arbitration, blobs→R2.

## Other CF primitives to pull in (from the review, behind existing seams)
- **Workflows / Agent "Fibers"** — the durable optimization/eval loop (consumers *of* the §0 log; keep them OUT of the synchronous append path).
- **AI Gateway** — managed-tier observability/usage capture + caching. Complements, does NOT replace, the credential boundary (it observes/caches, doesn't scrub secrets).
- **Workers AI + Vectorize** — the small/local-model-worker tier + the kypp memory vector store, each behind kypp's swappable store seam (Vectorize splits storage local↔managed — only when kypp goes managed).
- **GSV is reference prior art, not shared infra** (`deathbyknowledge/gsv`, third-party — nothing to coordinate or merge). Positive evidence for the `extends Agent` + custom `ctx.storage.sql` + never-`this.setState` skeleton, and for cribs documented in `docs/managed-tier.md` (a hibernate-safe pending-op routing table, versioned SQL migrations, a typed req/res/sig frame envelope, R2 compaction). **The defining divergence — don't crib:** GSV has *no sandbox/container at all* — it runs the agent loop in the DO and dispatches tool calls to the *user's own connected devices*. pillbox is the opposite: a disposable isolated Container with a real PTY, DO as sequencer-only. And GSV has *no §0 log* — negative evidence that the sequenced/attributed/replayable log is pillbox's daylight.

## Run the local smoke (no CF account, no deploy)

`wrangler dev` runs real `workerd` + real DO SQLite locally.

```sh
npm install
npx wrangler dev            # http://127.0.0.1:8787
# in a second shell (Node >= 22):
node smoke.mjs http://127.0.0.1:8787 spike-sess-1
```

**Pass criteria:** each POST returns a strictly increasing `seq`; a `from=1`
subscriber replays seq 1-3 on connect then tails seq 4 on the open socket, in
order, no gap/dup. Restart `wrangler dev` + re-subscribe `from=1` → full replay
returns (seq persisted to SQLite). A curl + websocat equivalent is in the
managed-tier notes.

## §0 contract mapping (DO ⟷ pillbox)

The DO is the **same surface** as `src/events/log.rs::SessionLog`, different
placement. `src/contract.ts` mirrors `contract.rs::Event` (camelCase, `type`-tagged
payload). The seam: producers **never self-assign seq** — they submit `seq=0`
(`Event::session` already does) and the authority stamps. Local foreground/detached
runs hold the append lock in the host/sandbox process; managed runs have the DO
hold it, and the in-sandbox §0 producer's sink swaps from "append `log.jsonl`" to
"POST `/event` to the DO" — one transport swap, same Event, same builder. Readers
(`subscribe`/`watch`/`wait-idle`) gain a "DO-WS source" alongside the local file
source with no schema change. The clean Rust seam is an `EventLog` sink trait with
`JsonlSessionLog` (local) + `ManagedDoSink` (POST) impls.

## Current CF API basis (mid-2026, GA unless noted)

The Agents SDK (GA Apr 2026) wraps the underlying DO primitives this relies on:
DO SQLite storage (`ctx.storage.sql`, `transactionSync`; 10 GB/object, 2 MB/row),
single-threaded-per-id (~1k req/s), WebSocket Hibernation (the SDK's
`onConnect`/`onMessage` + `connection.setState`/`getConnections()` over
`acceptWebSocket`/`serializeAttachment`), and instance addressing (`this.name`).
The Sandbox SDK (GA Apr 2026, PTY-over-WS/exec/backup-restore) is the container
layer this spike stubs — its `Sandbox` is itself a DO, so this Session Agent is a
sibling/wrapper, not its host.

## Risks + the next slice

- **Replay/tail race** — an append racing a fresh subscribe. The `cursor` guard
  (`fanout` sends only `seq > cursor`; cursor set before `onConnect` returns)
  closes dup + gap, but the smoke should add a stress variant (append-in-a-loop
  while subscribing) before trusting it.
- **Cost / managed-detach** — the DO hibernates to ~$0 idle, but the *container*
  doesn't; an idle detached session needs an idle-TTL that suspends/snapshots
  the container (Sandbox SDK backup/restore) while the DO + log persist. That's
  a container-lifecycle policy, not a §0 mechanic — out of spike.
- **Next slice (recommended): sandbox wiring** — make `/input` real (getSandbox →
  drive PTY-over-WS → in-container tailer POSTs `/event` back with `seq=0`),
  completing milestone 0 and measuring the one unmeasured risk: the DO↔container
  hop latency/cost. Alternative slice: the broker (actor-auth from the connection
  + driver-token arbitration + the deploy-mid-drive handoff probe).
