---
id: session-event-log-spine
project: pillbox
type: decision
status: active
title: One per-session append-only log is the durable spine (§0 keystone)
related_code:
  - "src/events/log.rs"
  - "src/contract.rs"
  - "src/events/transcripts/**"
---

<!-- brief:anchor session-event-log-spine -->
## The per-session `log.jsonl` is the single durable, replayable spine

Every session's events land in one append-only `log.jsonl`
(`src/events/log.rs::SessionLog`) keyed by `sessionId`, with a per-session
monotonic `seq`, a **co-located single-writer** (the log itself, seq recovered on
open, append flock-locked against concurrent writers), and `read_from(seq)` /
`subscribe(from, stop, sink)` replay. This is the §0 keystone: the substrate's
local stream every consumer reads (inner-loop readout, fleet triage, lum, later
multiplayer).

**Why.** One attributed, replayable log is simultaneously the observability
surface, the multiplayer spine, and the eval dataset — "replay the log, reduce to
messages" — so a single durable structure serves all three instead of three
parallel emit paths. The log-on-disk is the durable identity; a session survives
sandbox replacement.

### Invariant
- `seq` is monotonic **per session** (not per-emitter — the legacy per-run/exec
  counter is explicitly legacy; the "monotonic per pillbox" comments were wrong
  and are corrected).
- `seq == 0` / ephemeral events bypass the durable log (live-only telemetry) and
  are excluded from replay.
- The append path holds a lock so concurrent writers cannot collide on `seq`.
- `Payload::Unknown` exists so an older consumer can replay a newer log without
  failing (cross-version forward-compat).

> **Scope (backfill 2026-07-01) — what is built vs specced.** BUILT: the
> single-writer per-session log, `sessionId`/`seq`/`actor`/`Payload::{Unknown,
> Input,Annotation}`, transcript producer, `Usage` wire/native source-of-truth.
> **NOT yet built** (specced in `docs/session-event-log.md`): envelope
> `causationId` and `idempotencyKey` (absent from `contract.rs`); the
> content/`signal` `class` field on the envelope (only present on the `Artifact`
> payload today); the `raw_body` blob store (bodies are dropped —
> see `vault-egress-default-deny` sibling and `genai_tap.rs`); gateway-assigned
> seq. Those land with multiplayer / pooling, not preemptively.
