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

## Risks

- **CF lock-in** — the coordination layer becomes DO-shaped; mitigate by keeping
  the §0 log/seq/actor contract portable (it's just the envelope) and the
  trait seam clean.
- **"No moat" reality** — the substrate is
  commoditized; this tier only matters if the §0/multiplayer/circulation layer
  on top is the product. If we end up reselling CF's sandbox, we've lost.
- **Scope creep** — multiplayer/roster/arbitration is a large surface. The
  milestones gate it behind a falsifiable spike; honor the stop after milestone 0.
