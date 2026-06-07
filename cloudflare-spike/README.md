# Cloudflare Durable-Object-as-§0-gateway — spike

A runnable `wrangler dev` spike proving the one load-bearing claim of the managed
tier ([docs/managed-tier.md](../docs/managed-tier.md)): **a per-session Durable
Object is the §0 sequencer + Subscribe fan-out, 1:1 with pillbox's local
`SessionLog`.** Everything else (sandbox/container, vault/broker, actor-auth) is
stubbed so the spike stays small.

> **✅ Validated live (2026-06-07)** — locally on `wrangler dev` AND **deployed on
> real Cloudflare, free tier** (`https://pillbox-do-spike.vuluan-a06.workers.dev`):
> `node smoke.mjs <url> <sid>` → append assigns monotonic seq 1-3, a `from=1`
> subscriber replays 1-3 then tails 4, in order, no gap/dup. **No Workers Paid
> needed** — SQLite-backed DOs run on the free plan, so the §0 sequencer + fan-out
> (the load-bearing claim) is proven in production for free. Two fixes are in-tree:
> the Agents SDK needs the `nodejs_compat` flag (it imports node built-ins), and it
> **multiplexes its own `cf_agent_*` control frames onto the WS** — a §0 subscriber
> filters to Event envelopes (`seq`+`payload`); a real consumer does the same. The
> lock-free seq (single-threaded-per-id + `transactionSync`) holds as designed.
>
> **Two deploy configs:** `wrangler.toml` = **free / §0-only** (no container;
> `/input` is append-only — the attributed-input §0 path without the exec hop).
> `wrangler.container.toml` = the **full** path (Sandbox container; `wrangler deploy
> -c wrangler.container.toml`) — needs **Workers Paid** (Containers entitlement) and
> builds linux/amd64 (so it deploys to CF even from an arm64 host, unlike local
> `wrangler dev`). The container leg is validated to the hop boundary; full
> container *execution* is the only Paid-gated piece.

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

## The DO is the resident-sequencer EventLog impl

`SessionGateway` (this DO) and `src/events/log.rs::SessionLog` (the local
"no-daemon" impl) are two **placements of the same EventLog contract**
(docs/session-event-log.md §Sequencing names them: local single-writer, shipped;
resident sequencer, the DO). Same surface, same seq-authority rule, different
backing store.

| EventLog method | SessionLog (Rust, local single-writer) | SessionGateway (TS, DO resident sequencer) |
|---|---|---|
| `append` (seq authority) | `SessionLog::append` — stamps next seq from `last_seq`, **overwrites** the producer's seq | `append()` — stamps `MAX(seq)+1` inside `transactionSync`, **ignores** any body-supplied seq |
| `read_from(from)` | `SessionLog::read_from` — replay `seq >= from` | `readFrom(from)` — same query over the SQLite `log` table |
| `subscribe(from)` | `SessionLog::subscribe` — `read_from` then tail via `notify` on `log.jsonl` | `onConnect` — `readFrom` replay then live tail via `fanout` over the hibernatable WS |

**Seq-authority parity (the load-bearing invariant): both overwrite the
producer's seq — the log is the authority, never the producer.** SessionLog
recovers `last_seq` from `log.jsonl` on open and advances it in memory;
SessionGateway derives `MAX(seq)+1` from DO SQLite inside `transactionSync`
(single-threaded-per-id → lock-free monotonicity). The producer submits `seq=0`
(`Event::session` already does) and the placement stamps; swapping a local run
for a managed run is one transport swap (append `log.jsonl` → POST `/event`),
same `Event`, same builder.

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

## Actor attestation (the trust boundary)

Every event carries an `actor` (`docs/session-event-log.md` §Actor model). It is
**stamped from a verified credential, never the request body** — so authz (who
may drive/approve/join) can key off it. The credential is an HMAC-signed actor
claim (`src/auth.ts`): the issuer signs `{kind,id,display}` with a shared secret,
the DO verifies it server-side and stamps the *verified* actor.

| Path | Credential | Policy |
|---|---|---|
| `POST /event` | `Authorization: Bearer <token>` | write requires a valid token (401); body `actor` ignored; **control payload types rejected (403)** — `driver_changed`/`input`/`scored` have their own authoritative paths, so the open producer channel can't forge them |
| `POST /input` | `Authorization: Bearer <token>` | write requires a valid token (401); body `actor` ignored; gated by driver arbitration (below) |
| `subscribe` (WS) | `?token=<token>` on the upgrade | open to anonymous readers; a valid token binds the actor to the connection (`WsState.actor`) for future socket-driven input |
| container-hop exec result | — | stamped `system` (the gateway originated it) |

**`system` is the gateway's own identity** (it stamps `SYSTEM_ACTOR` for exec /
arbitration events): a token claiming `kind:"system"` is **rejected** — only
`human`/`agent`/`service` are token-borne, so a holder can't forge gateway events.

Set the secret out-of-band (never committed): `wrangler secret put
ACTOR_TOKEN_SECRET` for deploys, or `.dev.vars` (gitignored; see
`.dev.vars.example`) for `wrangler dev`. No secret → writes fail closed.

Verify: `node test-auth.mjs` (crypto unit — round-trip, wrong-secret,
tampered-sig, swapped-claim, **system-token rejected**) and, against `wrangler
dev`, `node smoke-actor.mjs` (401 without a token; body-claimed actor ignored;
**system-token → 401, forged `driver_changed` → 403**).

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
