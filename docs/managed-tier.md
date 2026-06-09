# Managed tier — durable sessions on Cloudflare (design)

Status: **design / proposed** (2026-06-06). The differentiated layer above the
local substrate. Depends on the §0 spine ([session-event-log.md](./session-event-log.md))
and realizes the gateway ([gateway.md](./gateway.md)) as the placement authority.
Sibling to [vnext.md](./vnext.md) (owns the layering + sequence).

> **Direction.** pillbox is local-only today (Docker + libkrun behind the
> `SandboxBackend` trait). "Remote" returns here, in a **different shape than
> the deleted ssh/e2b/`docker://` URL backends**: not a transport to someone
> else's daemon, but a **managed placement** behind the same trait, where a
> Cloudflare **Durable Object is the per-session gateway** and a Cloudflare
> **Container runs the agent**.

> **Research-backed (2026-06-06).** The shape below was pressure-tested by a
> deep-research pass (`wfg349iml`, 21 sources, 25/25 claims confirmed under
> adversarial verification). Verdict + numbers + the corrected electric.ax read
> are in [§Verdict](#verdict--research-backed) and [§Sources](#sources). A few
> genuinely-open items remain tagged **OPEN** at the end.

---

## Thesis — build *above* the commoditized substrate, don't reimplement it

Cloudflare (and CF + Anthropic's managed agents) already shipped the raw
substrate: a Sandbox SDK (PTY/exec/WS/R2-fs), Containers, an Agents SDK, and a
secret-injecting credentials proxy (≈ our vault). Reimplementing any of that is
how we lose (CF shipped the substrate — build above it).

The daylight is the **coordination layer**: the per-session §0 log + sequencer +
multiplayer broker + attach — the thing *every* consumer subscribes to,
**above** their sandbox. So the managed tier = our §0/gateway layer running on
their placement, not our placement competing with theirs.

## The keystone insight — the gateway *is* a Durable Object

The §0 spec defines the gateway as: *"one sequencer per session = the session
gateway/broker. Co-located producers submit; the gateway assigns `seq` on append
→ total order, deterministic replay,"* plus roster/auth/input-arbitration plus
attach.

That is the definition of a **Durable Object**: a single-writer, co-located,
addressable actor with durable (SQLite) storage and hibernatable WebSocket
fan-out. The two abstractions are the same.

Consequence: the multiplayer primitives the §0 spec **deferred for lack of a
local consumer** become load-bearing *exactly* here, and only here.

| §0 deferred primitive | Why deferred locally | Why load-bearing in the DO |
|---|---|---|
| **`actor` on the envelope** (gateway-authenticated, not self-reported) | "No current single-player consumer" | The DO authenticates each WS connection → stamps `actor`. The trust boundary the spec requires *exists* once there's a broker. |
| **gateway-assigned `seq`** (vs per-emitter today) | Co-located single writer already total-orders | The DO is the sole writer; multiple remote producers/humans submit, DO assigns. The canonical multi-writer case. |
| **roster / driver-token / `input` arbitration** | One human, one PTY | Multiple humans on one session need it. |

So "start the managed tier" and "land the §0 multiplayer keystone" are **one
move**, on the substrate where the keystone first earns its keep.

### Build on the Agents SDK `Agent` class, not raw `DurableObject`

The Session DO **extends the Cloudflare Agents SDK `Agent`** (GA Apr 2026), not a
bare `DurableObject`. The `Agent` *is* a DO subclass — a strict superset — so it
gives the hibernatable WebSocket lifecycle (`onConnect`/`onMessage`),
per-connection state (`connection.setState`), `getConnections()`/broadcast, and
`this.schedule()` (the container idle-TTL primitive) for free, while the keystone
`transactionSync` + `MAX(seq)+1` append survives **verbatim** (a DO primitive the
Agent inherits — adopting `Agent` costs the sequencer nothing).

**Backed by the CF Agents docs + corroborated by external prior art:** GSV (a
third-party gateway at `~/code/gsv` — *not* ours) runs this exact pattern
(`Kernel`/`Process extends Agent`, custom `ctx.storage.sql` Store classes,
`connection.setState` per-connection, **never** global `this.setState`) — a
useful worked example to crib the DO skeleton + `this.schedule`/connect-auth shape
from, nothing to coordinate with.

**The one conflict, declined:** the Agents SDK's global `this.setState` is
last-write-wins, whole-object, auto-synced — structurally incompatible with an
ordered append-only log keyed by monotonic `seq` (it'd lose ordering, replay, and
per-event `actor`). So the §0 log lives in `ctx.storage.sql` (our table, no sync),
the subscriber cursor in `connection.setState` (per-connection, no global sync),
and `this.setState` is **never called**. The SDK helps everywhere and fights only
on the convention we opt out of. (Spike: `cloudflare-spike/`.)

**Other CF primitives** (behind existing seams): **Workflows / Agent Fibers** for
the durable optimization loop (consumers *of* the §0 log, never the append path);
**AI Gateway** for managed-tier observability/usage + caching (complements, does
*not* replace, the credential boundary — it observes, doesn't scrub); **Workers
AI + Vectorize** for the small-model-worker tier + kypp memory (behind kypp's
swappable store seam).

### Cribbed from GSV (third-party prior art — `deathbyknowledge/gsv`)

GSV is the closest worked example on this exact substrate (`Kernel`/`Process
extends Agent`, custom `ctx.storage.sql` stores, `connection.setState`, never
`this.setState`, `this.schedule`). Reviewed @ `4b71a789` (v0.2.3). It is *not*
ours and *not* shared infra — study-only. Two kinds of signal:

- **Negative evidence = our daylight, confirmed.** GSV has **no §0 analog** — no
  append-only log, no monotonic seq authority, no replay-from-cursor, no
  per-event `actor`, no driver-token. Durability is per-`Process` conversation
  state (replaced/compacted, not appended-with-replay) + ephemeral `sig` pub/sub
  (no persistence, no replay). So the sequenced + attributed + replayable
  multiplayer log is genuinely unclaimed even here — pillbox builds it; GSV is no
  shortcut, only confirmation it's the daylight.
- **Patterns to adopt** (each ties to a milestone):
  - **Hibernate-safe pending-op routing table** (GSV `kernel/routing.ts`): persist
    every in-flight cross-boundary call `(id → origin, target, expiresAt)` in DO
    SQLite + a drop-sweep that *fails* pending ops when a connection/device dies
    or the DO is evicted mid-call. This is the discipline the **DO↔Container hop**
    needs and Milestone 0/1 currently hand-waves — a driven `session send` whose
    container reply is in flight at hibernation must be reaped + failed loudly, not
    just retried under `idempotencyKey`. **Add to Milestone 0/1.**
  - **Versioned SQL migrations** (GSV `kernel/schema/runner`) over inline `CREATE
    TABLE IF NOT EXISTS`: the §0 log table *will* gain `actor`/`class`/blob-refs/
    `idempotencyKey`. A tiny migration runner in `onStart()` now avoids later
    `IF NOT EXISTS` archaeology. (See [session-event-log.md](./session-event-log.md) §Versioning.)
  - **Typed `req/res/sig` frame envelope** (GSV `protocol/frames.ts`): typed
    args-by-call-name for `/input` + `/event` instead of `body as {...}` casts;
    drive = `req`→`res` (correlation id + retryable error codes), §0 fan-out = the
    `sig` channel. (See [gateway.md](./gateway.md) §Submit-wire-contract.)
  - **App-session multi-client model** (GSV `app-session.ts`: one session, N
    clients, per-client secret, `active|detached|…`) for `session share`/roster
    (Milestone 4); **run-queue-while-busy** (GSV `Process` `message_queue` + run
    phases) for the turn-queue half of driver arbitration; **capability-glob authz**
    keyed off the authenticated `actor` (who may drive/approve/join);
    **signal-watch-with-TTL** (GSV `SignalWatchStore`) for off-session attention
    fan-out (the `dx.md` detached-approval loop) without a resident socket.
  - **Compaction-to-R2 with generation pointers** (GSV `Process` archives cold
    segments to R2) validates Milestone 3's "DO keeps seq + recent tail, cold
    segments → R2".

**The defining divergence — do NOT crib:** GSV has **no sandbox/container at
all.** It runs the agent loop in the `Process` DO and dispatches tool calls
(`shell`/`fs`) as syscalls over WebSocket to the *user's own connected devices*
(BYO-trusted-machine OS). pillbox is the opposite: a **disposable, isolated CF
Container** running `claude`/`codex`/`opencode` under a real PTY (the runner
image + libkrun/Docker isolation + the MITM vault + snapshots), with the DO as
sequencer-only. GSV is therefore positive evidence for the DO-as-Agent skeleton
and the cribs above, but **no guide for the sandbox/PTY/credential layer** — and a
cautionary tale on scope (its `kernel/do.ts` is a ~2900-line god-DO spanning
identity/pkg/apps/git; keep the Session DO single-purpose: log + seq + roster).

---

## Verdict — research-backed

**Sound, and it's Cloudflare's own shipped + officially-recommended pattern.**
The Sandbox SDK *is* a Durable Object per sandbox that owns a container's
lifecycle and proxies to it; CF's "Rules of Durable Objects" explicitly endorses
one-DO-per-coordination-unit (chat room, game session, document, tenant). DO
gives exactly the properties §0 needs: globally-unique-by-name, one
single-threaded active instance, co-located local-disk SQLite, WebSocket
hibernation (32,768 conns/instance, zero GB-s while idle).

So milestone 0 is **not** "will it work" — that's confirmed. The de-risking is
about **bounded resources + authority handoff**, all design-arounds, not blockers:

| Risk | Number (CF docs) | Design-around |
|---|---|---|
| Per-DO SQLite cap | **10 GB** (GA; was 1 GB beta) | A per-session log is bounded by the session and fits; **blobs (`raw_body`/`pty_snapshot`) → R2**, never DO. If a long log nears the cap, offload cold segments to R2, DO keeps seq + recent tail. |
| Single-writer throughput | **~1,000 req/s** simple, 200–500 complex, single-threaded | Fine for dozens of humans on a low-write coding session; not for high-fanout. Saturation point is **OPEN** (below). |
| Deploy restarts every DO | disconnects **all** WebSockets | Clients **reconnect-and-replay-from-seq** (`subscribe --from SEQ` already does this). |
| In-memory state lost on eviction | ~70–140 s idle | **Persist the seq counter to SQLite** — never keep it only in memory. |
| Instance migration mid-session | uniqueness re-enforced on next storage access (stale instance throws) | A **storage-backed sequencer never double-commits** — the single-writer guarantee holds across migration. |

Crucially, **the DO is the sole `seq` authority** — the container *submits*
events (`seq=0`), the DO *assigns*. So the happy path is **not** distributed
total-order-under-partition (the spec's feared hard case); it's a single writer.
The residual hard bit is only the **handoff window** during DO migration/deploy
for a *live* attached session (ordering is safe; driver-token arbitration across
the window is the asserted-not-proven part — see [§Open](#open-questions-genuinely-unresolved)).

---

## Architecture

```
                    ┌─────────────────────────────────────────┐
   humans / lum /   │  Session Durable Object  (the gateway)   │
   IDE / CI / orca  │  ──────────────────────────────────────  │
        │  WS attach │  • §0 log (DO SQLite)  ← single seq      │
        ├───────────▶│  • actor auth (from the WS connection)   │
        │            │  • roster + driver-token arbitration     │
        │   §0 events│  • attach fan-out (WS hibernation)       │
        ◀────────────│  • blob refs → R2 (raw_body, pty snaps)  │
                     └───────────────┬──────────────────────────┘
                          submit §0  │  ▲ drive (input)
                          (seq=0)    ▼  │
                     ┌──────────────────────────────────────────┐
   the agent  ───────│  Cloudflare Container  (the placement)    │
   actually runs     │  runner image: pillbox-init + pty-host +  │
   here              │  agent (claude/codex/opencode) + vault?   │
                     └──────────────────────────────────────────┘
```

- **DO = gateway/sequencer/broker/attach.** Holds the durable §0 log, assigns
  `seq`, stamps `actor`, brokers participants, fans out attach. Coordinates; it
  does **not** host the agent process (a DO can't run a long-lived shell).
- **Container = placement.** Runs the runner image (same `pillbox-init` +
  pty-host + agent we run locally). Producers inside submit §0 events to the DO
  with `seq=0`; the DO assigns. This reuses the libkrun reparented-producer model
  (`__session-tailer`) — the producer ships to the DO instead of appending a
  local file. **This split is the CF Sandbox SDK's native architecture** (its DO
  owns the container lifecycle via `ctx.container.start()/destroy()/signal()/
  monitor()` and routes requests; same sandbox id → same DO) — so we reuse it.
  What's *additive* is the §0 sequencer + actor-auth role: the SDK's DO owns
  lifecycle/cache, **not** a per-event log/sequencer. That's our layer.
- **Both behind the `SandboxBackend` trait.** A `ManagedBackend` joins
  `DockerBackend` + `LibkrunBackend`; `select_backend()` gains a managed arm. The
  session record gains a `placement` so attach/teardown route correctly.

### Placement behind the trait

```rust
// src/sandbox/mod.rs — today
trait SandboxBackend { fn run(&self, …) -> Result<…>; /* + reattach/kill */ }
fn select_backend() -> Box<dyn SandboxBackend> { /* docker | libkrun */ }
```

The managed backend is a third impl. The CLI surface is **placement, not
transport**: e.g. `pillbox run --managed` (or a `[placement]` profile), never a
URL to a daemon. Everything the local trait already does (launch, reattach,
kill, the §0 producer) maps onto DO RPCs.

### §0 in the Durable Object

| Local today | Managed |
|---|---|
| `events/log.rs::SessionLog` → `<pillbox>/sessions/<id>/log.jsonl` | DO SQLite table `log(seq PK, at, actor, payload, …)` — bounded per session, fits under the 10 GB cap; offload cold segments to R2 only if a log nears it |
| co-located single-writer assigns per-session seq, recovered on open | DO is the single writer; `seq` from a **storage-persisted** counter (never in-memory-only — survives eviction) |
| `subscribe(from, stop, sink)` via `notify` file tail | `subscribe(from)` over the DO WebSocket; DO replays `read_from(seq)` then tails. Clients **reconnect-and-replay-from-seq** across deploys (every deploy disconnects all WS) |
| blobs deferred | `raw_body`/`pty_snapshot`/large outputs → **R2**, content-addressed; DO stores only refs (these are what would blow the SQLite cap) |

The envelope (`contract.rs::Event`) is already the right shape: `sessionId`
partition key, `seq` left 0 for the authority to assign (`Event::session(...)`),
`Payload::Unknown` forward-compat. Managed adds the two deferred fields —
`actor` (DO-stamped) and a DO-assigned `seq` — **without changing producers**:
they already submit `seq=0` and let the authority stamp.

### Attach + drive

- **Attach** = a WS to the DO. The DO replays the log from the requested seq,
  then tails — late-join replay is free (the spec's design goal). Frame
  transport (raw PTY) stays out of the durable log (the §0 rule); the DO relays
  the live frame stream + periodic `pty_snapshot` blobs for late joiners.
- **Drive** = `input` events submitted over the same WS; the DO arbitrates
  (driver-token for `target:pty`, queue for `target:agent` turns) and forwards
  to the container's pty-host. This is `session send` over the DO instead of
  `docker exec`.

### Credentials / vault — re-weighted by the research

**The vault is no longer a differentiator.** CF + Anthropic shipped *Claude
Managed Agents* (May 2026) with a secret-injecting outbound proxy that injects
secrets into requests *outside* the sandbox so the agent never sees them — i.e.
our vault is now a managed CF/Anthropic product. Two paths:

1. **Reuse CF's secret-injecting proxy.** Less to build; cedes the boundary to CF.
   Caveat (Pluto Security): it protects only *vault-stored* credentials — env
   vars, mounted files, and the system prompt stay visible *inside* the sandbox.
2. **Run our MITM vault in the container** (the libkrun L5/L6 model) — keeps
   scrub / egress-fence / token-readback ours.

Lean **(1) for the managed tier** to avoid rebuilding a commoditized boundary,
**unless** parity testing shows it misses token read-back or strict-deny egress
(both of which the pooling story needs). Don't anchor differentiation on the
vault — the daylight moved to the §0/multiplayer/eval layer (see [§Verdict](#verdict--research-backed)).

### Sync model — DO+WS vs ElectricSQL Durable Streams

The instinct that electric.ax is "for chat agents, not us" was *half* right and
worth correcting precisely. ElectricSQL is **not** Postgres-CRUD-only: its
**Durable Streams** (Dec 2025) / **Hosted Durable Streams** (Jan 2026) are a
generic *append-only-log-over-HTTP* primitive — opaque monotonic offsets,
offset-based late-join replay, SSE tailing — **structurally the same shape as
§0**. They even published *"Durable Sessions — the key pattern for collaborative
AI"* and a multiplayer-AI-chat demo. So it's a real candidate, not a category
error.

**But it doesn't give the two things that make §0 §0:** a single-writer
**logical `seq`** and **per-event `actor` attribution** — Durable Streams expose
opaque *byte*-offsets, so the sequencer + attribution + driver-token arbitration
would be **app-layer additions on top**. And its headline pitch (WS/SSE are
ephemeral, no position-resume) is *weak against §0 specifically*, which already
does position-resume (`subscribe --from SEQ`).

**Decision: DO+WS is primary.** The DO gives the single-writer seq, the
authenticated `actor`, *and* input/driver arbitration **natively** — exactly the
parts Electric would make us rebuild. Durable Streams stays a noted alternative
*transport* (durable, cacheable HTTP; resumable) worth revisiting if WS-on-deploy
churn or read-fan-out cost bites — but adopting it reintroduces the very
sequencer/attribution layer we'd be offloading. (Same verdict rules out
Yjs/CRDTs — wrong model for a single-authority append-only log — and the managed
sync engines Liveblocks/PartyKit/Convex/Zero as heavier than a per-session DO.)

---

## What we reuse vs. build

| Reuse (CF or ours) | Build (the daylight) |
|---|---|
| CF Container + Sandbox SDK (sandbox, PTY, exec, R2 fs) | The **Session DO**: §0 log + seq authority + actor auth |
| The runner image (`pillbox-init`, pty-host, agents) — unchanged | The **`ManagedBackend`** trait adapter + `select_backend` arm |
| `contract.rs` Event/Payload envelope + frame protocol | **actor + DO-assigned seq** (the deferred §0 primitives) |
| The reparented-producer pattern (`__session-tailer`) | **Producer→DO submit** transport (replaces local file append) |
| rustic content-addressed store *concept* | **R2 blob store** for `raw_body`/`pty_snapshot` (+ content/signal class) |
| Our MITM vault (recommended) | **Roster / driver-token / input arbitration** (multiplayer broker) |

Non-goals: reimplementing CF's sandbox/container/PTY; local multiplayer; the
distributed seq-reconciliation hard case (below) in v1.

---

## Competitive map & leverage list

Two deep-research passes (`wfg349iml` CF/sync; `w41xdzm8y` competitors) plus the
**smolvm** find converge on one read: **the substrate is triple-commoditized**
— Cloudflare (managed), Claude Managed Agents (the vault as a product), and now
**smolvm**, an Apache-2.0 OSS libkrun stack that is essentially pillbox's own
L1–L7 — while the **single-writer-sequenced + per-event-actor-attributed +
multiplayer-attach log for coding agents is contested-but-UNCLAIMED**: confirmed
absent in the leading event vocabulary (AG-UI), the closest competitor (AWS
Bedrock AgentCore), and smolvm. So: **rent everything below the daylight; build
only the daylight.**

| Player | Tier / what it is | Leverage | Tag |
|---|---|---|---|
| **smolvm** (smol-machines, Apache-2.0, 3.5k★) | our libkrun substrate as OSS — libkrun-FFI + smoltcp 0.13 + a **vsock guest-agent** control plane + OCI (no daemon) + egress allowlist + ssh-agent forwarding (keys never enter guest) + GPU (Venus) + an **embeddable Rust crate** (`EmbeddedRuntime`: create/start/connect/exec/`exec_streaming_with(ExecEvent)`/read+write_file/ports) | **Adopt as the local microVM backend** behind `SandboxBackend` → retire hand-rolled libkrun L1–L7 (they solved the libkrunfw build + codesign + GPU we hand-rolled). Gate: run our pty-host + §0 producer over their vsock/exec/port channel (their guest-agent is theirs; §0/attach/vault stay ours). `.smolmachine` = rootfs/layer pack, **not** a live snapshot → rustic stays our fork-from-store. Same worldview (local-first, library-not-daemon, isolation-default) — **soul brother: collaborate/partner.** | adopt + partner |
| **e2b** (Apache-2.0, Firecracker) | raw sandbox + pause/resume (fs+mem) + `connect()` reconnect; self-hostable infra repo | Alt self-hostable sandbox+snapshot primitive behind the trait | integrate |
| **Morph** (proprietary) | sandbox + branch/snapshot + browser remote-desktop | Managed fork primitive only; "sub-250ms snapshot" claim **unverified** | monitor |
| **AWS Bedrock AgentCore** (closed) | **closest competitor** — Runtime + Memory + Observability + Harness | Adopt its OTEL per-step trace/span schema; exploit gap = local-first + cross-backend + **true multiplayer-attach** (it's per-session isolation, not multi-human watch-and-drive) | monitor + adopt-schema |
| **DBOS Transact** (MIT, embeddable) | durable-execution engine (Postgres-checkpointed, auto-resume from last step) | Embed under managed runs for resumability without a separate orchestrator | integrate |
| **CF Workflows** (GA) / **LangGraph `BaseCheckpointSaver`** | durable execution / checkpointer interface | CF Workflows for the managed tier; conform §0 to `BaseCheckpointSaver` (put/get_tuple/list/…) → consumable by LangGraph persistence | integrate / adopt-iface |
| **AG-UI** (MIT, CopilotKit; adopted by Google/AWS/MS/LangChain) | agent↔user event protocol, transport-agnostic (WS/SSE) | Adopt the ~16–25 typed event taxonomy (Lifecycle / Text / ToolCall / Reasoning / State) for the §0 payload vocab — but it has **no seq, no per-event actor, no multiplayer** (exactly our daylight, confirmed absent) | adopt-vocab |
| **OTEL GenAI semconv** | observability standard | Emit §0 as OTEL GenAI so it's consumable by Langfuse / Phoenix / Jaeger — no silo | adopt |
| **Zed ACP** | cross-agent editor protocol (Claude/Codex/opencode/…) | Cross-agent portability — but **local/stdio only**, NOT a remote-attach envelope (the remote-attach claim was refuted) | monitor |
| **Inspect / LangSmith / Braintrust** | eval / trajectory-replay | Emit a schema they ingest → the **§0-log-as-eval-dataset** (our other daylight) consumable, not siloed | adopt-schema |

**Net — keep three things ours, rent the rest:** the sequenced + attributed +
multiplayer §0 log, cross-backend §0 portability (local Docker/libkrun/smolvm +
managed), and the eval/optimization loop over the log. Cheapest ship into the
daylight: adopt AG-UI vocab + OTEL semconv + a durable engine, and stand on
smolvm/e2b for the substrate, spending the build budget only on the daylight.

> **Scan caveats.** Mostly vendor docs, not at-scale postmortems; some claims are
> prove-a-negative. **Genuinely-open:** the multiplayer-attach UX of
> Devin/Factory/Cursor/Replit/Amp/OpenHands (a Devin "async-PR-only" claim was
> *refuted* — don't assume async-PR is universal), the exact eval-ingestion
> schema, and Temporal/Inngest/Restate/Convex embeddability. Worth a focused
> follow-up before betting the multiplayer UX.

---

## The sequencing model — narrower than the spec feared

The §0 spec braced for the "managed-placement disconnect case … distributed
total-order under partition." The research shrinks it: **the DO is the sole
`seq` authority** (the container submits `seq=0`, the DO assigns from a
storage-persisted counter), and DO single-writer semantics make a
**storage-backed sequencer never double-commit** even across instance migration
(uniqueness is re-enforced on the next storage access; a stale instance throws).
So the happy path is one writer, not a consensus problem.

- **v1: synchronous submit, back-pressure on unreachable DO.** Producers submit
  to the DO and **block** rather than mint provisional seqs — the agent stalls
  loudly instead of forking the order. A DO outage drops ordering authority;
  documented, not silently wrong. No provisional-seq reconciliation in v1.
- **The residual risk is the handoff *window*** during a DO deploy/migration for
  a *live attached* session. Ordering is safe (storage-backed); but
  reconnect-and-replay-from-seq covering the **input/driver-token arbitration**
  cleanly across that window is **asserted-not-proven** — milestone 0/1 must
  probe it (a deploy mid-drive shouldn't double-grant the driver token).

---

## Coexistence & migration

- Local backends (Docker, libkrun) are unchanged and remain the default. Managed
  is **opt-in**.
- `Session` gains a `placement` (`local` | `managed`); attach/reattach/kill/the
  status projection route on it. (Note: we just removed the always-`"local"`
  `Session.remote` field — `placement` is a *real* dispatch axis, not a display
  label, so it earns a field where `remote` didn't.)
- §0 readers (`subscribe`/`watch`/`wait-idle`/`ingest`/`log`) are already
  transport-agnostic over the log shape; they gain a DO-WS source alongside the
  local file source.
- Blob store at rest is a new sensitive surface — add the R2 row to
  [security.md](./security.md) **before** the capture path lands (the §0 spec's
  cutover requirement).

---

## Milestones — spike first, measure twice

| # | Milestone | Proves |
|---|---|---|
| **0** | **Seam spike** — `ManagedBackend::run` places a session on a CF Container running the runner image (reuse the Sandbox SDK's DO-owns-container); a Session DO holds the §0 log in SQLite, assigns `seq` from a **storage-persisted** counter; `session subscribe`/`watch` attach over the DO WS with **reconnect-and-replay-from-seq**; one driven turn round-trips (`send` → agent → `§0` reply streamed back). Single human. | The trait seam + §0-over-DO + synchronous submit. Limits are confirmed safe; this proves *latency + cost* of the DO↔container hop. |
| 1 | **actor + auth + handoff probe** — DO stamps `actor` from the authenticated WS connection; `wait-idle`/status project from the DO log; **deploy-mid-drive test** (driver token must not double-grant across a DO restart). | The trust boundary; the deferred §0 field; the one asserted-not-proven risk. |
| 2 | **Credentials** — managed run with token read-back; decide CF-proxy vs our MITM (lean CF-proxy unless it misses read-back/strict-deny). | Credential parity, minimal rebuild. |
| 3 | **Blobs → R2** — `raw_body`/`pty_snapshot`/large outputs to R2 (keeps the DO under the 10 GB cap), content/signal `class`, GC tied to ttl/prune. | Persisted traces + the pooling primitive, where storage is cheap. |
| 4 | **Multiplayer** — roster + driver-token + `input` arbitration; web-attach (`session share`). | The circulation demo; the actual differentiator. |

Stop after 0 and re-evaluate before committing to 1-4. The spike measures the
two things the research left open (DO↔container hop cost; handoff window) — both
cheap to falsify.

---

## Consume path — a real agent through the gateway (scoped 2026-06-09 from the built spike)

The spike is now built **past** the Milestone-0 table's aspiration. `cloudflare-spike/`
has a `SessionGateway` DO that already does the §0 plane end-to-end: durable log +
storage-persisted **seq authority** (`append`, 1:1 with `SessionLog::append`),
`subscribe` (replay-then-tail over the DO WS), **actor attestation** (`auth.ts` HMAC),
**driver arbitration** (`ensureDriver`/`handleRelease`), and a **DO↔container exec
round-trip** (`/input` → `driveSandbox` → `sandbox.exec` → `tool_call` §0 event → a
subscriber sees input→output in order). All smoke-tested (`smoke-{actor,driver,sandbox}.mjs`);
the contract (`contract.ts`) is **parity-gated** against `contract.rs`
(`check-contract-parity.py` in `cf.sh`). The §0/trust/subscribe substrate is **done**.

What it runs in the box is `echo`, not an agent. Closing that — the **consume path** — is
the realization of Milestones 0–1, and it's smaller than the table implied: **one method
changes** and the whole §0/trust/subscribe substrate is reused untouched. (Consume, don't
rebuild: opencode runs in CF's Sandbox SDK; we stay the §0/memory/multiplayer layer above.)

**The seam.** `driveSandbox` is the only method that evolves. Today: `sandbox.exec(cmd)` →
one `tool_call`. Consume: drive opencode + tail its SSE → mapped agent §0 events.

| # | Piece | Detail |
|---|---|---|
| 1 | container image | Sandbox-SDK image with opencode installed + authed (the runner-image equivalent); the SDK runs `opencode serve` |
| 2 | drive | gateway health-checks the opencode port (`getSandbox`), POSTs the prompt to opencode `/session/{id}/prompt` (the structured API, **not** a shell exec). Driver-gated by `handleInput` already |
| 3 | **tail + map** (the new work) | gateway opens opencode's `/event` SSE (streaming fetch to the container port), runs a TS `OpencodeMapper` per event → §0 `Payload` → `this.append(at, AGENT_ACTOR, …)`. Fan-out unchanged |
| 4 | turn-done | opencode `session.idle` → `message_end` + `attention_required` → the §0 "turn done" (parity with local `wait-idle`; the read side already exists) |

**The mapper** is the one genuinely-new module — a TS port of `src/events/opencode.rs`
(~150 lines of logic): `message.updated`→`message_start`; `message.part.delta`→
`message_delta`/`thinking`; `message.part.updated[tool]`→`tool_call`, `[step-finish]`→
`usage` (deduped by step id); `session.idle`→`message_end`+`attention_required`;
`permission`/`question.asked`→`attention_required`. State: open-message-id + emitted-step-ids.

**Trust boundary — reused, not rebuilt.** Agent output is stamped `{kind:"agent",
id:"a:opencode"}` **by the gateway** (add an `AGENT_ACTOR` const beside `SYSTEM_ACTOR`),
never self-reported by opencode-in-the-container — the exact boundary `handleEvent`
enforces. Driver `/input` stays human/service; `driver_changed` stays system.

**Risks / decisions:**
1. **The mapper becomes a 2nd implementation** (Rust local + TS CF) — a drift surface
   exactly like the contract we just gated. De-risk: reuse the Rust mapper's SSE→Payload
   test vectors (`opencode.rs` tests) as the TS mapper's fixtures, and extend
   `check-contract-parity` to "same SSE fixture → same §0 payloads, both sides." The
   natural sequel to the contract-parity keystone.
2. **The DO holds a long-lived SSE for the whole turn** (minutes), mapping+appending as
   events arrive — vs. today's bounded one-shot exec. **This is Open Question #1 (DO↔container
   hop cost) for a *streaming* turn — the thing the spike must measure**, plus mid-turn
   eviction.
3. **opencode auth in the container = the vault parity** → consume CF's (Managed Agents'
   secret-injecting proxy / `wrangler secret`), don't rebuild our MITM (Milestone 2).
4. **`contract.ts` additions**: the mapper emits `message_start`/`thinking`/`usage`
   (currently catch-all-absorbed, rust-only) — model them explicitly so parity field-checks
   them. `usage` especially (kypp credit-assignment / cost-routing consume it).

**Cheapest falsifier (one real turn).** Evolve `smoke-sandbox.mjs` from `echo` to a real
agent turn: drive opencode with a trivial prompt; a WS subscriber must see `message_start
→ message_delta… → message_end → attention_required`, in order, `actor=a:opencode`. Proves
opencode-in-Sandbox + SSE tail + TS mapper + §0 append + subscribe end-to-end, and directly
measures risk #2. If it holds, the managed tier is real; if not, the wall is found cheaply.
**Stays non-P0** — a spike to de-risk the option, not a tier build-out.

## Resolved by the research

- **DO-as-session-gateway is sound** — it's CF's own shipped pattern (Sandbox
  SDK = DO-per-sandbox owning a container) and recommended granularity. *Was OPEN #1/#2.*
- **DO limits** are design-arounds, not blockers (10 GB cap → blobs to R2;
  ~1k req/s single-writer → fine for a low-write session; persist the seq;
  reconnect-replay on deploy). *Was OPEN #1.*
- **Sync model** — DO+WS primary; electric.ax/Durable Streams is §0-shaped but
  lacks single-writer seq + actor attribution, so it'd reintroduce our layer.
  *Was OPEN #3.* (See [§Sync model](#sync-model--dows-vs-electricsql-durable-streams).)
- **Defensibility** — the substrate (sandbox + container + credential proxy) is
  commoditized (Claude Managed Agents, May 2026). The defensible layer is
  **attributed multi-actor drive/attach + cross-backend §0 portability (local
  Docker/libkrun + managed) + the optimization/eval layer that consumes the
  durable log** — *not* the sandbox or the vault. *Was OPEN #4.*

## Open questions (genuinely unresolved)

1. **DO↔container hop cost at sustained drive** — given the 20:1 WS request
   billing ratio + the ~1k req/s single-writer ceiling, at what event-rate /
   attach-count does one per-session DO saturate? (Milestone 0 measures.)
2. **Handoff window** — does reconnect-and-replay-from-seq fully cover
   input/driver-token arbitration during a DO deploy/migration on a *live*
   session, or are there windows it can violate? (Milestone 1 probes.)
3. **Competitor positioning on the durable-session axis** — the research did
   *not* close where e2b / Modal / Daytona / Fly / Vercel Sandbox / Morph /
   Runloop sit on durable-session + multiplayer-attach vs. raw sandbox. Is the
   §0/attribution/multiplayer layer genuinely unclaimed? Worth a focused scan.
4. **Lock-in** — is the `SandboxBackend` trait enough to keep the managed tier
   swappable, or does DO-shaped coordination leak everywhere?

> **Evidence caveat.** Most sources are CF/Electric *vendor* material (docs,
> blogs, announcements), not independent at-scale postmortems; throughput
> numbers are idealized; the Sandbox SDK is still **beta**, not GA. The verdict
> is "architecture-as-documented is sound," which the spike must confirm under
> our workload. The landscape shifted in a ~6-month window (Durable Streams Dec
> 2025, Hosted Jan 2026, Claude Managed Agents May 2026) — short shelf life.

## Sources

- Cloudflare — [Project Think (agents-on-DO economics)](https://blog.cloudflare.com/project-think/) ·
  [Rules of Durable Objects](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/) ·
  [DO limits](https://developers.cloudflare.com/durable-objects/platform/limits/) ·
  [WebSockets/Hibernation](https://developers.cloudflare.com/durable-objects/best-practices/websockets/) ·
  [in-memory state](https://developers.cloudflare.com/durable-objects/reference/in-memory-state/)
- Cloudflare Sandbox SDK — [repo](https://github.com/cloudflare/sandbox-sdk) ·
  [architecture (DO owns container)](https://developers.cloudflare.com/sandbox/concepts/architecture/) ·
  [GA/beta status (InfoQ)](https://www.infoq.com/news/2026/04/cloudflare-sandbox-ga/)
- [Claude Managed Agents (the vault, commoditized)](https://blog.cloudflare.com/claude-managed-agents/)
- ElectricSQL — [Durable Streams](https://electric-sql.com/blog/2025/12/09/announcing-durable-streams) ·
  [Hosted Durable Streams](https://electric-sql.com/blog/2026/01/22/announcing-hosted-durable-streams) ·
  [Durable Sessions for collaborative AI](https://electric-sql.com/blog/2026/01/12/durable-sessions-for-collaborative-ai)
- Sync-engine comparisons — [ElectricSQL/Convex/Zero guide](https://merginit.com/blog/24082025-sync-engines-guide-electricsql-convex-zero) ·
  [Liveblocks/PartyKit/Hocuspocus](https://www.pkgpulse.com/guides/liveblocks-vs-partykit-vs-hocuspocus-realtime-2026)
- [AI code sandbox benchmark 2026](https://www.superagent.sh/blog/ai-code-sandbox-benchmark-2026) ·
  [AI sandbox pricing (Northflank)](https://northflank.com/blog/ai-sandbox-pricing)

---

## Considered: Cloudflare Artifacts as the managed workspace store (deferred 2026-06-08)

When the managed store question came up ("push the workspace to R2/S3"),
[Cloudflare Artifacts](https://developers.cloudflare.com/artifacts/) — versioned,
**Git-compatible** file-tree storage (closed beta) — looked like a better
interface than R2 + a hand-rolled CAS layout, for the *workspace-snapshot* half
specifically.

**The fit (real):** snapshot = commit, bookmark = branch/ref, `pillbox fork` =
git clone/branch; the whole `push/pull/snapshot/bookmark/fork` surface maps onto
git verbs, **with real 3-way merge** (which rustic can't do). It's reachable
three ways — git-over-HTTPS, REST, and a **Workers binding** — so the managed
Container `git push`es and the Session DO reads the result tree via the binding
(no separate S3 client; tighter than R2). **ArtifactFS lazy-mount** ("mount large
repos without full clone") natively closes the eager-restore-vs-lazy-fault-in gap
the [fork substrate](#) plan was going to hand-roll with overlayfs warm-base.
Limits: 10 GB/repo, 1 TB/account, 2000 req/10s.

**Why deferred (two blockers):**

1. **It breaks store transferability — the portability moat.** Artifacts is a
   *different* CAS engine with a git face; it does **not** ride on R2 and is **not**
   "push your rustic repo to R2." So a snapshot lineage spanning local (rustic) and
   managed (Artifacts) has no shared handle space — a rustic snapshot handle ≠ a
   git commit SHA — and would need a translation/sync layer between two CAS engines.
   That sync is exactly the thing the local-first identity is supposed to avoid, and
   it cuts against "the same integrated bundle runs identically across local / BYO /
   managed" (portability is the moat — see the fork-substrate decision). Also: no
   client-side encryption (CF-hosted git is readable by CF), unlike rustic's
   client-encrypted store with `workspace rekey`. So at best it's the **managed-only**
   `WorkspaceBackend` impl behind the trait, never a replacement for local rustic —
   and the two-store split is the cost, not a feature.

2. **Closed beta — can't even trial it.** No access means we can't falsify the
   DO↔binding read path, the lazy-mount behavior on real agent workspaces (git is
   poor at large binaries / node_modules), or the merge story. Nothing to spike.

**Decision:** document, don't adopt. The managed store stays **R2** (plain blobs
for §0 `raw_body`/`pty_snapshot`, milestone 3) plus whatever the managed
`WorkspaceBackend` materializes; local stays **rustic**. Revisit Artifacts when
(a) it leaves closed beta *and* (b) we have a concrete answer for cross-store
lineage that doesn't reintroduce a two-engine sync — otherwise the transferability
loss outweighs the nicer git interface.

---

## Risks

- **CF lock-in** — the coordination layer becomes DO-shaped; mitigate by keeping
  the §0 log/seq/actor contract portable (it's just the envelope) and the
  trait seam clean.
- **"No moat" reality** — the substrate is
  commoditized; this tier only matters if the §0/multiplayer/circulation layer
  on top is the product. If we end up reselling CF's sandbox, we've lost.
- **Scope creep** — multiplayer/roster/arbitration is a large surface. The
  milestones gate it behind a falsifiable spike; honor the stop after milestone 0.
