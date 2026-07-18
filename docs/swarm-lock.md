# Swarm lock — a mutex that hands off context (design)

Status: **idea / design** (2026-07-18). Not built; nothing here is ratified.
A coordination primitive for **agents sharing mutable resources** — the
`sandbox spawn/exec/agent` swarm shape, not the isolated fork-`k` shape.

Companion to [session-event-log.md](./session-event-log.md) (the §0 spine the
events land on), [vault-oauth-refresh-coordination.md](./vault-oauth-refresh-coordination.md)
(the `TokenStore` — pillbox's first swarm mutex, minus the handoff), and
[swarm-memory.md](./swarm-memory.md) (the external loop that would mine the
handoff journal).

---

## The one-line idea

> A plain mutex only transfers *exclusion*. A swarm lock also transfers
> *context*: the holder declares **intent** on acquire, leaves a **handoff
> note** on release, and every waiter unblocks already knowing *what was done
> and why* — the serialization point doubles as the knowledge-transfer point.

The insight is that a mutex's critical section is exactly the span a waiting
agent has zero visibility into. Two agents that touch the same migration
directory don't just need mutual exclusion — the second agent needs to know the
first one renamed the table it was about to alter. The lock already *is* the
synchronization point where that information must flow; today it just doesn't.

## Where it applies (and where it deliberately doesn't)

pillbox has two swarm shapes with opposite contention stories:

| Shape | Workspace | Contention answer |
|---|---|---|
| `dispatch -k` fork-`k` / segments | **CoW fork per worker** from a bookmark (`commands/dispatch.rs` — each worker is `run --from-bookmark --detach`) | **isolation** — workers can't conflict by construction; the verdict picks a winner. No locks needed, ever. Do not add them here. |
| orchestrator-driven swarm (`sandbox spawn/exec/agent`, `session send`) | **shared** — many agents against one workspace, or against genuinely-shared cross-session resources (the OAuth token family, a dev database, a deploy slot) | **serialization** — today: nothing. The one built case is the vault `TokenStore` flock, hand-rolled for one resource. |

The swarm lock generalizes the second row. Resources are **named, advisory**
strings — a path by convention (`src/db/migrations/`), or an abstract resource
(`deploy:staging`, `schema:api`). Advisory like `flock`: it coordinates
cooperating agents, it does not enforce against a non-cooperating one
(FS-level enforcement would mean virtiofs interception — out of scope, see
Open questions).

## Precedents already in the tree (this is not a new species)

1. **The §0 log append lock** — `src/events/log.rs::SessionLog` append is
   flock-serialized so concurrent writers can't collide on `seq`
   (the `session-event-log-spine` decision). Single-writer-via-file-lock is
   established house style.
2. **The vault `TokenStore`** ([design](./vault-oauth-refresh-coordination.md))
   — an exclusive flock + holder metadata + generation counter serializing N
   brokers on one refresh-token family. It is precisely a swarm mutex over the
   resource "the OAuth token family," including bounded lock-wait, owner
   metadata for wedged holders, and a crash-safe pending marker. What it lacks
   is the *handoff*: the loser learns the new generation, not what happened.
3. **Driver-token arbitration** — `driver_changed { from?, to, mode:
   granted|requested|stolen|released }` in the §0 payload taxonomy is lock
   arbitration over the resource "the PTY," with the exact mode vocabulary a
   general lock needs (including `stolen`). The managed-tier DO already runs
   this arbitration live (`ensureDriver`/`handleRelease`).

The swarm lock is the third precedent generalized to arbitrary resources, plus
the context handoff the first two never needed.

## Design

### Authority placement — per-pillbox lock store, not the per-session log

A swarm's agents are (today) **separate sessions**, and the §0 log is
per-session — so the per-session log cannot arbitrate a cross-session lock.
The authority is a host-side, daemon-free lock store (the `TokenStore`
placement, the `gateway-no-daemon` grain):

```
<pillbox>/locks/<sha256(resource)>/
  holder.json      # {resource, holderSessionId, actor, pid, acquiredAt, ttl,
                   #  intent, generation}   — written atomically under the flock
  journal.jsonl    # append-only handoff chain: request/grant/release/expire/steal
  lock             # the flock target — the critical section
```

- **Serialization** = exclusive `flock` with bounded wait (single-host is
  sufficient — pillbox is local-only; the managed tier gets a DO instead,
  below).
- **The journal is the bus.** Waiters tail `journal.jsonl` with the same
  notify-tail pattern `subscribe` uses on `log.jsonl` — consistent with the
  ratified "fan-out = read-side readers of the file" model; no push path, no
  daemon.
- **Session logs are projections, not the authority.** On grant/release the
  acting side *also* appends the event to its own session's `log.jsonl` (with
  its authenticated-locally `actor`), so replay, `session diagnose`, and
  attribution see the lock activity in-thread. The lock store is the truth;
  the per-session copies are correlation.

### The handoff protocol

```
acquire(resource, intent, ttl):
  flock (bounded wait; on timeout → return the CURRENT holder.json + tail of
         journal so the caller can report "blocked on X, who is doing Y")
  append journal: {mode: granted, actor, sessionId, intent, causation: <request seq>}
  write holder.json
  return: the journal tail since the caller's last acquire of this resource
          — the accumulated handoff chain. THE WAITER UNBLOCKS HOLDING CONTEXT.

release(resource, note, blob?):
  verify caller == holder (or --steal, journaled as such)
  optional: snapshot-diff enrichment (below)
  append journal: {mode: released, note, blobRef?, resultSnapshot?}
  clear holder.json, drop flock       → every tailing waiter wakes with the note
```

- **`intent` on acquire** (required, one line) — visible to anyone blocking or
  running `lock status`, so waiting *is* informative: "blocked on
  `schema:api`, held by `a:claude@sb-3` — *adding pagination params to
  /v2/list endpoints*."
- **`note` on release** (required; `--file` for a longer body) — the "what was
  done and in what context." Inline note stays small; a bigger body goes to
  the **existing blob store** and the journal keeps the ref — exactly the
  `Artifact` discipline (`summary` inline, body `blobRef`, never inlined).
- **Waiter intents are visible to the holder** (`lock status` shows the
  queue + intents), so a holder can leave a *targeted* note — it knows who is
  waiting and for what. Cheap, and it's the difference between a changelog
  entry and an actual handoff.

### Self-report hardening (the Goodhart caveat, addressed structurally)

The release note is agent-authored — trusting it alone re-opens the
self-stamped-status hole that `session score` exists to close. So the release
event carries **both channels**:

- **claimed** — the note (judgment, cheap, possibly wrong);
- **mechanical** — an optional `resultSnapshot` ref: `release` can cut a
  workspace checkpoint (the snapshot machinery exists) so the journal entry
  pins *what actually changed* next to *what the holder says changed*.

Waiters read the note; auditors and the memory loop diff the two.

### §0 payload additions (additive, forward-compatible)

New `Payload` arms (new `oneof` arms appended in `agent.proto`; older readers
hit `Payload::Unknown` — the spine's versioning rule holds):

| Payload | Fields | Actor |
|---|---|---|
| `lock_changed` | `resource`, `mode: requested\|granted\|released\|expired\|stolen`, `intent?`, `note?`, `blobRef?`, `resultSnapshot?`, `holder?` | the acquirer/releaser; `system` for `expired` |

One payload with a `mode` (the `driver_changed` shape) rather than five —
the journal line and the session-log projection share this type.

### Failure modes

- **Holder dies (sandbox destroyed, pid gone) or wedges** — the lock is a
  **lease**: `ttl` with optional heartbeat-extend. On expiry the reaper (any
  waiter's bounded-wait path — no daemon) journals `mode: expired` and clears
  the holder. Crucially the waiter unblocks knowing **the chain is broken**:
  there is no note, and that absence is itself surfaced ("previous holder
  expired mid-work; last intent was X; inspect before proceeding" — plus the
  workspace state to diff). A silent mutex would just… unlock.
- **Stale pid / OS-released flock ≠ logical release** — `holder.json` is the
  logical state, the flock only serializes writers to it (the `TokenStore`
  lesson: release-on-death does nothing for a live-but-wedged holder; owner
  metadata + lease expiry handle that).
- **Deadlock** — v0 punts on multi-lock ordering: bounded wait + `ttl` means
  no wait is unbounded; `lock status` shows the full wait graph for a human or
  orchestrator to break with `steal`. A lock-ordering discipline (or
  wound-wait) is a later problem — see Open questions.
- **Steal** — allowed, loudly: `mode: stolen` journals who, from whom, and
  why. Same philosophy as driver `stolen`.

### Surface

```sh
pillbox lock acquire <resource> --intent "…" [--wait|--nowait] [--ttl 15m]
pillbox lock release <resource> --note "…" [--file handoff.md] [--snapshot]
pillbox lock status [<resource>]          # holder + intent + waiter queue
pillbox lock log <resource>               # the handoff chain (the good stuff)
pillbox lock steal <resource> --reason "…"
```

v0 callers are **host-side**: the orchestrator brackets the work it dispatches
(acquire → `session send` the task with the inherited context prepended →
release with the worker's summary). In-guest agents come second, via the
**shared-MCP surface** — two tools (`acquire_lock`, `release_lock`), honoring
the tiny-tool-surface rule in [swarm-memory.md](./swarm-memory.md); the MCP
server is host-side so the guest never touches the lock store directly.

### Managed-tier symmetry

Same table as the credential design — one invariant, two substrates:

| | Local | Managed (Cloudflare) |
|---|---|---|
| Arbitration | per-resource `flock` + `holder.json` | a **Resource DO** (a DO *is* a single-writer actor; driver arbitration already runs this way) |
| Wake-up | notify-tail on `journal.jsonl` | DO `subscribe` fan-out (built) |
| Journal | `journal.jsonl` | DO storage / R2 |

### The sleeper payoff — `lock log` is per-resource episodic memory

The handoff journal is a **per-resource changelog with intent attached**:
every entry is (who, why they touched it, what they did, pointer to the real
diff), with provenance (session id + seq) for free. That is *exactly* the
episodic raw material the [swarm-memory](./swarm-memory.md) distiller wants,
already scoped and already gated on "someone actually did work here" — no
transcript mining required. A resource that accumulates a long journal is
also a signal in itself (a contention hot-spot ≈ a module that wants
decomposing).

## What v0 is NOT

- **Not enforcement** — advisory only; a non-cooperating agent can still write
  the file. (Enforcement = virtiofs/overlay interception; separate, large.)
- **Not for dispatch fork-`k`** — isolation already wins there.
- **Not a daemon** — flock + files + notify-tail, per the no-daemon grain.
- **Not a distributed lock** — single host; the managed tier gets the DO, and
  the two never mix authority (a session is placed on exactly one substrate).

## Open questions

1. **Granularity + hierarchy** — is `src/db/` a prefix-lock over `src/db/*`?
   Prefix semantics are tempting and a tarpit (lock `src/` and you've built a
   global lock). v0: exact-string resources, convention over mechanism.
2. **Multi-lock ordering** — punt (bounded wait + ttl) vs. enforce canonical
   acquire order vs. wound-wait. Punt until a real swarm deadlocks.
3. **Context injection shape** — does the inherited journal tail land in the
   worker's prompt (orchestrator's choice, `session send`), as an `Annotation`
   the agent may read, or as an MCP resource? Leaning: orchestrator's choice
   in v0; the primitive returns the tail and stays unopinionated (substrate
   exposes, consumer decides).
4. **TTL default** — long enough for a real work unit, short enough that a
   crashed holder doesn't park the swarm. Probably per-acquire required, no
   global default, until usage teaches us.
5. **Should `TokenStore` retrofit onto this?** — same flock+holder+journal
   skeleton. Tempting for one-primitive elegance, but the token store's
   at-most-once `pending` protocol is domain-specific and correctness-critical;
   do not generalize it away. Revisit only after both exist.
6. **Waiter-queue fairness** — flock wake order is OS-arbitrary. FIFO via the
   journal (`requested` entries) is easy if unfairness ever bites.

## Relationship to ratified decisions (conformance, not amendment)

- `session-event-log-spine` — untouched: the lock store is a *separate*
  cross-session authority; session logs gain only additive projection events
  (`Payload` arm appended, `Unknown` fallback covers old readers). The
  per-session single-writer seq rule is not weakened.
- `gateway-no-daemon` — honored: no resident process; files + flock + tail.
- `libkrun-is-the-backend` / docker deprecation — the primitive is host-side
  and backend-agnostic, but its in-guest exposure lands on the microVM path
  only; nothing is built for docker.
