# Session event log (vNext keystone)

Status: design spec. Supersedes the split between the lifecycle stream
(`src/events/mod.rs`, `events.jsonl`) and the agent I/O contract
(`src/contract.rs`, `proto/pillbox/v1/agent.proto`) by unifying them on a
single per-session durable log.

Part of [vnext.md](./vnext.md), which owns the layering (session > run >
container) and the unified sequence. This spec is layer 1 — the session
spine. The session-vs-container reconciliation lives there.

## Why this is the keystone

One append-only, attributed, per-session log is simultaneously:

- **Harden / observability** — raw MITM bodies (incl. reasoning) and usage
  become first-class events; OTel / Raindrop become *exporters that read the
  log*, not a parallel emit path.
- **Multiplayer spine** — `actor` on every event + durable attributed input +
  participant/role/driver events + a single per-session sequencer give total
  order, late-join replay, and attribution.
- **Eval dataset** — "replay the log, reduce to messages" is the dataset
  export. No separate capture.

The architecture already points here: `Event.seq` is documented as
"monotonic … durable events only; ephemeral telemetry seq==0, excluded from
replay" — replay is designed in. We extend it; we don't replace it.

## Three planes (make the split explicit)

| Plane | Carrier | Holds | Durable? |
|---|---|---|---|
| **Event log (spine)** | `log.jsonl` per session | lifecycle, semantic agent output (messages/tools/phases), **attributed input**, annotations, participant/role/driver, usage, raw-body *refs*, periodic PTY *snapshots*, checkpoints | yes, replayable |
| **Frame transport (live)** | `src/attach/frame.rs` | raw PTY bytes for attached viewers, flow-controlled | no — reconstructed on attach |
| **Blob store** | `blobs/<sha256>` | raw bodies, PTY snapshots, large tool outputs | content-addressed |

**Rule: raw PTY bytes never enter the durable log** (too high-volume —
`Frame::Data` is full-screen repaints). The log keeps *periodic*
`PtySnapshot` checkpoints + the semantic stream. Late-join = latest
`PtySnapshot` (from blob) + live frame tail. Replay = log from seq 0 (or from
a snapshot seq), dereferencing blobs lazily.

## Envelope

Per line (JSON; protobuf-JSON faithful, camelCase, `type`-tagged payload —
same encoding rules as `contract.rs`).

```jsonc
{
  "v": 1,                       // schema version (per-line; bump on breaking change)
  "seq": 42,                    // monotonic per SESSION, gateway-assigned, durable-only
                                //   0 ⇒ ephemeral, excluded from replay (preserves contract.rs)
  "sessionId": "abc123def456",  // PARTITION KEY — the durable identity (outlives sandboxes)
  "at": "2026-05-29T13:37:00Z", // RFC3339
  "actor": {                    // NEW — who produced this event (gateway-authenticated)
    "kind": "human",            // human | agent | system | service
    "id": "u:vuluan",
    "display": "Luan"
  },
  // correlation — which sandbox/run/exec this happened in (a session spans many)
  "sandboxId": "sb-1",          // optional
  "runId": "run-1",             // optional
  "execId": "",                 // optional
  "causationId": 39,            // optional — seq of the event that caused this
  "idempotencyKey": "…",        // optional — per-event append dedup on retry (NEW; not on the proto Event today)
  "payload": { "type": "input", "text": "run the tests", "target": "agent", "mode": "turn" }
}
```

### Changes vs. today's `contract.rs::Event`

- **`sessionId` is the partition key** (was `sandboxId`). Sessions are
  cattle-not-pets durable identities; a session survives sandbox replacement,
  so `sandboxId`/`runId`/`execId` demote to optional correlation fields.
- **`seq` is per-session** and **gateway-assigned**. Today it is per-*emitter*
  — `EventEmitter` resets `seq` to 1 per run AND per exec; the "monotonic per
  pillbox" comments (`contract.rs:26`, `agent.proto`) are inaccurate and should
  be corrected in code. See *Sequencing*.
- **`actor` added** — the one field multiplayer can't live without.
- **`causationId` added** — links input→message, request→resolution,
  driver-request→grant. Cheap; powers the attributed thread view.
- **`idempotencyKey` added** to the envelope — per-event append dedup on retry.
  Today `idempotency_key` exists only on the `Spawn`/`SendInput`/`Exec` *RPCs*
  (`agent.proto`), **not** on the `Event` type; per-event dedup is unbuilt.

### Content vs. signal — the poolability split

Every payload and blob classifies into two buckets so the opt-in collective
only ever sees the poolable one (see [vnext.md](./vnext.md) §Data
principles):

- **content** — raw code, prompts, messages, tool I/O, `raw_body` blobs,
  bootstrapped demos. **Local-only; never egresses.**
- **signal** — task features + outcomes: `usage`, exit codes, test
  pass/fail, tool/phase *names* (not args), intervention/retry counts,
  latency, model id. **Poolable** after scrub.

Make this a field (`class: content | signal`) or a static table keyed by
payload type — either way **structural**, so "pool the metadata, not the
code" is enforced by the schema, not by remembering to redact. The
shareable *artifacts* (tuned instructions, policy params) are a third thing
the optimizer produces, not log events — they get their own scrub gate
(exclude few-shot demos).

### Ingestion is format-pluggable from day one

The trainset adapter and live producers must parse **foreign trace shapes**,
not just pillbox's own: Claude Code transcripts, Codex rollouts, *and*
arbitrary HF agent-trace datasets used as plumbing fixtures. A `TraceSource`
trait normalizes each into `LogEvent`s; the `Payload::Unknown` fallback
(see *Versioning*) absorbs variants the normalizer doesn't model. Step-1
requirement — retrofitting normalization after the schema sets is expensive.

## Actor model

```jsonc
"actor": { "kind": "human|agent|system|service", "id": "<stable id>", "display": "<optional>" }
```

- `system` — pillbox itself (lifecycle, sequencing, snapshots).
- `agent` — the coding agent's own output (`message_*`, `tool_call` carry
  `kind:"agent"`, e.g. `id:"a:claude@sb-1"`).
- `human` / `service` — input, annotations, approvals (`id:"u:…"` / `svc:ci`).

**Trust boundary.** Unlike today's `emitter` tag (`host`/`sandbox`), which the
code explicitly says is *not* an access-control signal (anything that can write
the env can set it), `actor` is **stamped by the gateway from the
authenticated connection**, never self-reported by the producer. Authz
(who may drive / approve / join) keys off `actor`, so it must be authenticated.

## Payload taxonomy

Existing `contract.rs` variants are kept verbatim. New ones below.

**Lifecycle** (folded in from `EventType`; `actor.kind = system`):
`session_started` · `session_completed` · `session_failed` · `session_dropped`
· **`session_blocked`** (NEW) — emitted the instant the agent hits a
permission/attention gate while no one is at the PTY: `reason`/`category`,
`pendingAction` + args, `ttl`, `defaultOnTimeout`, and a reversible-vs-
irreversible flag (irreversible = push/merge/result-finalize → hard-block, the
agent cannot self-approve). Powers the detached approval loop + fleet triage
(see [dx.md](./dx.md)). Today `EventType` has only the first four.
· `sandbox_provisioned` · `sandbox_ready` · `sandbox_destroyed` ·
`run_started` · `run_finished` · `run_failed`

**Semantic agent output** (kept; `actor.kind = agent`):
`message_start` · `message_delta` · `message_end` · `tool_call` ·
`phase_changed`* · `todos_updated`* · `attention_required`
(* typically ephemeral — `seq 0`)

**Human-in-the-loop:** `permission_requested` (agent) ·
`permission_resolved` (human/service; `causationId` → the request)

**Multiplayer (new):**

| Payload | Fields | Actor |
|---|---|---|
| `participant_joined` | `role` | the joiner |
| `participant_left` | `reason` | the leaver / system |
| `role_changed` | `targetActor`, `role` | owner |
| `driver_changed` | `from?`, `to`, `mode: granted\|requested\|stolen\|released` | system/owner |
| `input` | `text` *or* `data`(b64), `target: agent\|pty\|exec`, `mode: live\|turn` | human/service |
| `annotation` | `text`, `anchor?` | human/service |

`input` is the **durable, attributed** turn/steer — distinct from the live,
ephemeral `Frame::Input` PTY keystrokes. `annotation` is the async,
keyboard-free comment (Slack-thread style); optionally injected as agent
context.

**Harden / observability (new):**

| Payload | Fields |
|---|---|
| `raw_body` | `direction: request\|response`, `bodyRef`(sha256), `bytes`, `model?`, `turnId?`, `redactions: []` — the MITM full body incl. thinking; stored in blob store, **never inlined** (>60KB content stays out of spans) |
| `usage` | `inputTokens`, `outputTokens`, `cache*`, `costUsd?`, `model`, `source: wire\|native` — first-class; `wire` = MITM, `native` = CC-OTEL enrichment |
| `native_metric` | `name`, `value`, `source: claude_code_otel` — optional, secondary, often ephemeral |

**PTY bridge (new):**
`pty_snapshot` { `cols`, `rows`, `snapshotRef` } — periodic vt100 checkpoint
(the `Frame::Snapshot` payload), blob-stored, so replay/late-join skip the
byte stream. (`pty_resize` optional.)

**Workspace** (kept): `checkpoint` · `result_ready`
**Exec** (kept): `exec_started` · `exec_output` · `exec_exit`
**Valve** (kept): `custom`

## Sequencing & ordering

- **Co-located case (ship first):** one sequencer per session = the session
  gateway/broker. Co-located producers *submit*; the gateway assigns `seq` on
  append → total order, deterministic replay. The multi-writer problem is easy
  **only here**.
- **Remote-disconnect case (hard; defer + scope):** when the producer is in a
  remote sandbox and the host gateway can disconnect, sandbox-side `seq` is
  provisional and must be reconciled — that is distributed total-order-under-
  partition, the hardest form; it does **not** "disappear." Until reconciliation
  is designed, **gate remote multiplayer on host-side-only sequencing** and
  document that a host disconnect drops the ordering authority.
- **No sequencer exists today.** `EventEmitter` assigns `seq` per-*emitter*
  (resets to 1 per run/exec). §0 must make the gateway the sole seq authority
  and define how host- and sandbox-side lifecycle events (both emit
  `session.started` today) get a seq without gaps/dupes.
- **Ephemeral** (`seq 0` / `ephemeral:true`) bypasses the log — live-only
  telemetry (cards, phase flicker). Existing semantics, preserved.
- **Idempotency:** add a per-event `idempotencyKey` to the `LogEvent` envelope
  with a defined dedup window + restart behavior. NOTE: `idempotency_key` today
  is on the `Spawn`/`SendInput`/`Exec` *RPCs* only — **not** the `Event` — so
  per-event append dedup is **unbuilt**.

## Storage layout

```
<pillbox>/sessions/<sessionId>/
  log.jsonl          # append-only spine (0600)
  blobs/<sha256>     # content-addressed: raw bodies, pty snapshots, large outputs
  head               # last durable seq — fast append + replay-resume
```

Pluggable backend:

```rust
trait EventLog {
    fn append(&mut self, events: &[LogEvent]) -> Result<u64>; // returns last assigned seq
    fn read_from(&self, seq: u64) -> Result<impl Iterator<Item = LogEvent>>;
    fn latest_snapshot(&self, before: u64) -> Result<Option<(u64, BlobRef)>>;
}
```

Local-jsonl default (local-first, greppable). A Postgres impl lands for
team/remote mode (Aquifer parity) without changing producers. This extends
the existing `EventSink` trait with read/replay.

The global `events.jsonl` becomes a **lifecycle-only projection** derived from
per-session logs (keeps `session list` / `session events --follow` working);
the per-session log is the source of truth.

**`session list`/`info` must project status from the log.** Today `session list`
reads only `attached_pid` (PTY ownership), so done/failed/blocked/running look
identical — it must join against the log for a status + needs-attention column
(see [dx.md](./dx.md)). The same log enables **`pillbox session diagnose ID`** —
a collector-free post-mortem (failure reason + last N tool calls + pending gate
+ `pull this snapshot to reproduce`), a flagship feature the log gives almost
for free.

## Versioning / forward-compat

- `proto/pillbox/v1/agent.proto` stays canonical (consumers codegen). New
  payloads append as new `oneof` arms at the end; new envelope fields are new
  field numbers.
- **Add an `Unknown` fallback to the Rust `Payload` enum.** `frame.rs` already
  does this for frames (`Frame::Unknown` keeps a newer peer's tags from
  breaking decode); `Payload` currently has none, so an older consumer
  replaying a newer log would fail. Needed for cross-version replay.
- `v` envelope bump on breaking field-set change (matches `events.jsonl`
  discipline). Enums keep the `Unspecified` deserialize fallback already used.

## Migration from today's two systems

| Today | vNext |
|---|---|
| `contract.rs::Event` (sandbox-keyed, `EventEmitter` seq) | gains `sessionId` (partition), `actor`, `causationId`; `sandboxId` optional; seq per-session, gateway-assigned |
| `events/mod.rs::EventType` lifecycle + jsonl/webhook/otel sinks | lifecycle → `Payload` variants (`actor.kind=system`); sinks → **exporters/projections** that read the log |
| `emitter` (`host`/`sandbox`, self-reported) | subsumed by `actor` (authenticated) + optional `sourceSide` correlation attr |
| `attach/frame.rs` | unchanged as live transport; gains a producer writing periodic `pty_snapshot` events |

### Cutover plan (not just a table)

The table is the *target*, not the *transition*. Required before building:

- **Dual-write or one-shot backfill** from `events.jsonl` → per-session logs;
  decide which.
- **In-flight / detached sessions.** Sessions outlive the CLI and can be
  reattached; define what happens to a session *started under the old layout*
  when the new spine lands (the v0.5→v0.6 hard-reset precedent says layout
  transitions matter — decide: live migration or another hard reset).
- **Seq-semantics migration.** The per-emitter→per-session change silently
  changes the meaning of every replayed log; old logs may not be replayable by
  the new reducer (the `Payload::Unknown` fallback helps payload forward-compat
  but **not** the seq change). State whether old logs are readable.
- **Rollback path** if the spine regresses.
- **Blob store at rest.** `raw_body`/`pty_snapshot` blobs are a new sensitive
  surface: reuse the existing **rustic content-addressed store** (don't build a
  second), and spec at-rest encryption, access control, GC/refcount (tie to
  session `ttl` + `prune`), and `content`-vs-`signal` classification *at write
  time*. Add the blob-store row to `security.md` before the capture path lands.

## Open decisions

1. **Gateway placement for remote sandboxes** — host-side sequencer (simple,
   one authority) vs. sandbox-side provisional seq reconciled at the host
   (resilient to host disconnects, more complex). Recommend host-side first.
2. **Keep or retire the global `events.jsonl`** — recommend keep as a
   lifecycle projection for back-compat, source-of-truth moves to per-session.
3. **Blob GC** — raw bodies/snapshots grow unbounded; tie retention to the
   session `ttl` + `session prune`, and a `--no-bodies` profile knob.
4. **Input target arbitration** — `input{target:pty}` must route through the
   driver-token arbiter (see multiplayer spec); `target:agent` (turns) can
   queue. Driver-token vs turn-queue default is the one genuine product fork.
```
