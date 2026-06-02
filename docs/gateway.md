# Session gateway — design spec

Status: design / proposed (2026-05-30); **partially realized** (2026-05-31).
The primitive [vnext.md](./vnext.md) §0 actually gates on, pulled out of the
layering's "one component, build it once" hand-wave. Sibling to
[session-event-log.md](./session-event-log.md) (the log it writes) and
[attach-transport.md](./attach-transport.md) (the frame channel it serves).

**Built so far:** the **sequencer** exists as the *co-located single-writer*
`events/log.rs::SessionLog` (assigns the per-session `seq` on append) — not yet
a network process holding an append lock. The **attach endpoint**'s read side
ships as `src/gateway.rs` (`session subscribe` — a sync `tungstenite` WS server
streaming `Subscribe(from_seq)` as JSON), plus `session send` for input and
tail-while-serving (the gateway tails a live session's transcript→log while it
serves). **Not built:** the **broker** (participant auth / roster /
driver-token arbitration / the `actor` it stamps) — net-new; the submit→seq
*network* wire contract + the in-sandbox producer token + the host↔sandbox
seq-authority handoff. Today's gateway is single-writer, single-session,
localhost, unauthenticated.

**Not** the swarm orchestrator. The orchestrator is an *external* consumer that
decomposes goals and routes across many pillboxes over the proto contract; the
gateway is *internal*, **one per live session**, and the thing the orchestrator
(and lum, and a human at a shell) talk *to*.

**Not the meta-harness, either (terminology — 2026-06-02).** "Gateway" here means
the **multiplayer broker**: the I/O / collaboration plane (sequencer + roster +
driver-token + fan-out attach) — content-agnostic, multiplexing participants over
a session. It **must serve a bare coding harness with zero optimization**
(collaborating on a plain claude/opencode run is first-class). The
**meta-harness** is a *separate, orthogonal* layer — the optimization / behavior
plane (DSPy / GEPA / RLM / cost-routing / ACE memory) that changes what the agent
does (online: grows the vault MITM into a model router/rewriter; offline: a batch
consumer of the §0 trace log). Both compose over the standalone §0 substrate;
neither requires the other (all four of {bare, meta-harness} × {solo, multiplayer}
are valid). Don't fold them into one component.

## What it is — three roles, one process

| Role | What it does | Replaces today |
|---|---|---|
| **Sequencer** | sole assigner of the per-session monotonic `seq`; appends to the session log | `EventEmitter`'s per-run/exec counter (`contract.rs`) |
| **Broker** | authenticates participants, holds the roster, arbitrates input (driver-token) | nothing — net new |
| **Attach endpoint** | serves the live frame stream + `Subscribe(from_seq)` replay | `attach/host.rs` + `attach/relay.rs` (extended) |

They are one process because they share one piece of state: the **append
position** (`head`) of the session log. The sequencer assigns from it, the
broker's input events append through it, and the attach endpoint reads from it.
Splitting them would mean three things coordinating one write cursor.

## The no-daemon reconciliation (the load-bearing decision)

The review's sharpest objection: "a stateful, long-lived, multi-writer
coordinator that outlives containers is a resident broker — colliding with
pillbox's no-daemon identity." Resolution:

> **The durable spine is the log on disk, not the gateway.** The gateway is an
> **ephemeral single-writer that holds the append lock only while a session is
> active.** No session is "served" by a resident daemon; the next attach/spawn
> starts a gateway that resumes from `head`.

This already matches how detach works today: on foreground attach the **host
`pillbox` process** is the live side; on detach it **exits** and the
**sandbox-side `pillbox`** keeps the agent + pty-host alive (`attach/host.rs`,
`attach/relay.rs`). The gateway is just that live side, given a log to write
and a lock to hold. When nothing is attached and nothing is running, there is
no gateway — only `sessions/<id>/{log.jsonl, head}` at rest.

So "outlives containers" is satisfied by the **log**, not a process. Seq
authority is **"whoever currently holds the session's append lock,"** and that
moves with the live side.

## Lifetime & placement — who holds seq authority

| Mode | Gateway runs… | Seq authority | Notes |
|---|---|---|---|
| Local foreground | the `pillbox run` process | that process | dies with the run; trivial single-writer |
| Local detached | a per-session background process (extends today's detached path) | that process | reattach connects to its unix socket |
| Remote, attached | **host** `pillbox` | host | in-sandbox producer submits *to* the host (below) |
| Remote, detached | **sandbox-side** `pillbox` | sandbox | host exited; sandbox is the live side |

The hard transitions are **remote attach↔detach**, where authority *moves*
between host and sandbox. That is the one place distributed ordering bites
(see *Sequencing*); everywhere else there is exactly one writer at a time.

## Submit → seq wire contract

Producers do **not** self-assign `seq`. They submit; the gateway assigns.

```
Submit(SubmitRequest{
  session_id,
  events: [ LogEvent without seq, each carrying idempotencyKey ],
  producer_token,            // authenticates the producer → stamps `actor`
}) -> SubmitAck{
  assigned: [ {idempotencyKey, seq} ],   // echo key → assigned seq
  head,                                    // session head after append
}
```

- **Ordering:** the gateway assigns `seq` in receipt order under the append
  lock; `SubmitAck` returns the assigned seq per event so the producer can
  correlate. Total order per session is the receipt order at the single writer.
- **Idempotency:** every event carries an `idempotencyKey` (new on the
  envelope — today it exists only on the `Spawn`/`SendInput`/`Exec` *RPCs*).
  The gateway dedups within a window and across restart by scanning back from
  `head`; a retried submit returns the *already-assigned* seq, never a second
  append.
- **Ephemeral** (`seq 0` / `ephemeral:true`) bypasses the log and the lock —
  fanned straight to live subscribers, never persisted.
- **Read path is the existing proto:** `Subscribe(SubscribeRequest{ from_seq })`
  streams the log from a cursor (`0` = live tail). Local readers may read
  `log.jsonl` directly; remote readers go through the gateway. There is no
  separate historical-read RPC and none is needed.

`Submit` is the missing *write* RPC — today producers push to an in-process
`EventSink`; the gateway is the sink that also assigns seq and is reachable
over the wire.

## Actor authentication (the real trust boundary)

`actor` must be **stamped by the gateway from the authenticated connection**,
never self-reported (today's `emitter` host/sandbox tag is explicitly *not* a
trust boundary — anything that can write the env sets it). Per connection:

| Participant | Authenticates via | Becomes |
|---|---|---|
| Local human (owner) | unix-socket **peer credentials** (uid) — owns the process | `actor.kind=human`, owner role |
| In-sandbox producer (agent driver) | a **per-session producer token** injected at spawn over the *same delivery path as the vault stdin blob* | `actor.kind=agent` |
| Remote human (web/2nd attach) | a **join token** (scoped, TTL) minted by the owner/gateway | `actor.kind=human`, granted role |
| Service / sub-agent / CI | a **service token** | `actor.kind=service` |

The in-sandbox producer is the subtle one: it is itself a remote actor relative
to a host gateway, so it cannot be trusted by placement. It gets a secret at
spawn time (the vault already proves this delivery path —
`dispatch_vault_stdin_direct`) and presents it on `Submit`. The gateway maps
token → `actor`; a forged or absent token is rejected, not trusted as "sandbox."

## Broker — input arbitration

The broker is the gateway's write-side policy for **human/agent input**, and it
emits its decisions as log events so they replay:

- **Driver-token** (host-authoritative, à la tmux / Live Share — *not* a CRDT):
  one writer holds the `driver` lease for `input{target:pty}`; others observe.
  Lease request/grant/steal-after-timeout are `driver_changed` events.
- `input{target:agent}` (turns) can **queue** rather than require the token.
- Async `annotation` input takes no token (keyboard-free, Slack-thread style).
- Every accepted input is an attributed `input` event (the actor is the
  authenticated connection, not a claim in the payload). Tool-approval routing
  (`permission_*`) flows through the same auth.

## Sequencing — co-located vs. remote-disconnect

- **Co-located (ship first):** one process holds the append lock → one writer →
  total order is trivial. This covers local (all modes) and remote-while-the-
  authority-side-is-stable.
- **Remote disconnect (hard; defer + scope explicitly):** when authority must
  move host↔sandbox on attach/detach, use a **lease + fencing token**: the
  session record holds a monotonically-increasing `seq_epoch`; the side taking
  authority bumps it; appends carry the epoch; a stale writer (old epoch) is
  fenced off. On reattach the host **pulls the sandbox's appended tail** before
  resuming. Until this is built, **gate remote multiplayer on host-side-only
  sequencing** and document that a host disconnect drops the ordering authority
  for the duration.

This keeps the invariant **exactly one seq authority at any instant**; the only
complexity is the handoff, not concurrent writers.

## Crash recovery

The gateway is restartable from disk: on start it reads `head`, scans back far
enough to rebuild the idempotency window, and resumes appending. No in-memory
state is authoritative — a killed gateway loses only un-acked submits, which
producers retry under their `idempotencyKey`.

## What §0 must actually build (reframes the roadmap item)

§0 is **not** "promote the envelope." It is:

1. **The log + append lock + `head`** (`sessions/<id>/`), single-writer.
2. **The sequencer** — assign per-session `seq` on append; delete the per-run/
   exec `EventEmitter` counter as the authority (keep it only as a provisional
   local seq for remote submit).
3. **`Submit` (write RPC) + envelope `idempotencyKey`** with dedup.
4. **Actor auth** — unix peer-cred (local), spawn-time producer token
   (in-sandbox), join tokens (remote humans).
5. **Re-model `Session`** from 1:1-with-a-sandbox (`session.rs` embeds one
   `sandbox_id`) into a cross-sandbox spine keyed by `sessionId`.
6. **Merge the two event systems** (`contract.rs` rich-but-sandbox-keyed +
   `events/mod.rs` lifecycle-but-seqless) onto the one log.

Items 1–4 are the gateway proper; 5–6 are the spine it writes to. None of it
exists today — which is why §0 is the gate, and why the remotes collapse
(which needs none of it — detach already keys off the durable `Session.id`)
should ship *first*.

## Open questions

1. **Local detached gateway = a background process.** That is the closest thing
   to a daemon pillbox has. Acceptable because it is per-session and dies with
   the session — but confirm it doesn't drift into an always-on resident.
   (Note: local detached already forgoes `--vault` because the host proxy can't
   outlive the CLI — the gateway has the same "who stays alive" question.)
2. **Lock primitive** — OS file lock (`flock`) on `head` vs. a lockfile with
   pid/epoch. File locks don't survive across hosts (remote handoff needs the
   epoch fencing regardless).
3. **Submit transport** — a real gRPC `Submit`, or fold writes into the existing
   control channel the frame protocol already multiplexes? The latter avoids a
   second listener but couples write-plane to attach-plane lifetime.
4. **Backpressure** — a slow log/disk vs. a fast producer; does `Submit` block
   or shed (ephemeral-only) under pressure?
