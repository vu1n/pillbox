---
id: gateway-no-daemon
project: pillbox
type: decision
status: active
title: The gateway is an ephemeral single-writer, not a resident daemon
related_code:
  - "src/gateway.rs"
  - "src/attach/host.rs"
  - "src/commands/session/**"
---

<!-- brief:anchor gateway-no-daemon -->
## Seq authority is "whoever holds the append lock while a session is live"

The gateway (sequencer + — later — broker + attach endpoint) is an **ephemeral
single-writer that holds the session's append lock only while the session is
active**. The durable spine is the log on disk, not a process. When nothing is
attached and nothing is running, there is no gateway — only
`sessions/<id>/log.jsonl` at rest; the next attach/spawn resumes from `head`.

**Why.** The sharpest objection to the gateway was "a stateful, long-lived,
multi-writer coordinator that outlives containers is a resident broker — colliding
with pillbox's no-daemon identity." The resolution: the *log* outlives containers,
not a process, and seq authority moves with the live side (exactly how detach
already works — the host process is the live side on foreground; on detach it
exits and the sandbox-side keeps the agent alive). "Exactly one seq authority at
any instant" holds without a daemon.

### Invariant
- No always-on resident coordinator serves a session; the live-side process holds
  the append lock and dies with the session.
- On restart, the writer recovers from disk (`head` + the log), never from
  authoritative in-memory state.

> **Built vs specced (backfill 2026-07-01):** what ships in `src/gateway.rs`
> today is the **read side** — a synchronous `tungstenite` WS `session subscribe`
> server, single-writer, single-session, localhost, **unauthenticated**. The
> **broker** (participant auth / roster / driver-token arbitration / the
> `Submit` write RPC / gateway-authenticated `actor`) is **not built** — it is
> net-new and lands with multiplayer. `actor` is currently producer-stamped
> locally, not authenticated at a network boundary.
