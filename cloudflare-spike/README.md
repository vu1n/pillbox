# Cloudflare Durable-Object-as-§0-gateway — spike

A runnable `wrangler dev` spike proving the one load-bearing claim of the managed
tier ([docs/managed-tier.md](../docs/managed-tier.md)): **a per-session Durable
Object is the §0 sequencer + Subscribe fan-out, 1:1 with pillbox's local
`SessionLog`.** Everything else (sandbox/container, vault/broker, actor-auth) is
stubbed so the spike stays small.

## Why this is the right first slice

This is **milestone 0 minus the container hop** — it isolates the §0-over-DO
mechanics (seq persistence, hibernation replay+tail, reconnect-from-seq) before
paying for the container. The keystone finding from the research: a DO is
**single-threaded per id** and `ctx.storage.transactionSync` serializes appends,
so **seq monotonicity is lock-free** — `MAX(seq)+1` inside a sync transaction,
no distributed ordering, no lock primitive. The spec's feared "total-order under
partition" doesn't arise; the DO is the sole writer.

## What's IN vs OUT

**IN** — a per-session DO (`idFromName(sessionId)`) with SQLite `log`:
- `POST /s/<id>/event` → append + assign monotonic `seq` (storage-backed, eviction-safe) → `{seq, head}`
- `GET /s/<id>/subscribe?from=N` → WebSocket replay (`seq >= N`) then live tail, one Event/frame in seq order (hibernation-aware: per-connection cursor on `serializeAttachment`)
- `POST /s/<id>/input` → append an attributed `input` Event (same path → also fans out)

**OUT (stubbed, marked `// TODO`)**: sandbox/container wiring (no real agent), vault/broker, `actor` authentication (taken from the body, not the connection), driver-token arbitration, blobs→R2, the deploy/migration handoff probe.

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

DO SQLite storage (`storage.sql`, `transactionSync`; 10 GB/object, 2 MB/row),
single-threaded-per-id (~1k req/s), WebSocket Hibernation (`acceptWebSocket`,
`serializeAttachment`, `getWebSockets(tag)`), `idFromName`. The Sandbox SDK
(GA Apr 2026, PTY-over-WS/exec/backup-restore) is the container layer this spike
stubs — its `Sandbox` is itself a DO, so this Session DO is a sibling/wrapper.

## Risks + the next slice

- **Replay/tail race** — an append racing a fresh subscribe. The `cursor` guard
  (`fanout` sends only `seq > cursor`; cursor set before `handleSubscribe`
  returns) closes dup + gap, but the smoke should add a stress variant
  (append-in-a-loop while subscribing) before trusting it.
- **`ctx.id.name` readback** — verify it's populated for `idFromName` ids in your
  workerd; if empty, pass `sessionId` via a header (2-line change). Contract
  parity only, not load-bearing for the smoke.
- **Cost / managed-detach** — the DO hibernates to ~$0 idle, but the *container*
  doesn't; an idle detached session needs an idle-TTL that suspends/snapshots
  the container (Sandbox SDK backup/restore) while the DO + log persist. That's
  a container-lifecycle policy, not a §0 mechanic — out of spike.
- **Next slice (recommended): sandbox wiring** — make `/input` real (getSandbox →
  drive PTY-over-WS → in-container tailer POSTs `/event` back with `seq=0`),
  completing milestone 0 and measuring the one unmeasured risk: the DO↔container
  hop latency/cost. Alternative slice: the broker (actor-auth from the connection
  + driver-token arbitration + the deploy-mid-drive handoff probe).
