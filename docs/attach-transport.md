# Interactive attach transport — design notes

Status: draft (2026-05-27). Sibling to
[`agent-io-contract.md`](./agent-io-contract.md).

The primitive: **a faithful, reattachable, backend-agnostic interactive
PTY channel to a live agent session.** Where `agent.proto` is the
*PTY-free* structured channel for consumers that render semantics
(orchestrators, Slack, hermes, headless lum), this is the *PTY* channel
for consumers that render a real terminal (orca, interactive lum, a human
at a shell). The two are siblings, not alternatives — `agent.proto` line
21 already carves this out: "Interactive PTY … uses pillbox's existing
attach transport." This document **is** that transport, formalized.

pillbox owns the sandbox-side screen model so an embedder can treat a
remote agent session as a **local renderable object**: connect, get the
current screen, stream input/output, resize, detach, reconnect — across
docker / e2b / ssh with one interface.

## Shape decisions

- **Sandbox-side pty-host, not a local daemon.** The process that owns the
  real PTY + screen model lives *inside* the sandbox (it is `pillbox`
  itself in a new mode — already baked into the e2b template / installed
  on the VPS). Its lifetime is the session's lifetime, tracked by the
  existing `pillbox session` record. This is orca's daemon role without a
  resident service, preserving the "self-contained bundle, no daemons"
  identity.
- **One frame protocol over any byte pipe.** A backend supplies only a
  bidirectional byte pipe; the *frames* on it are identical everywhere.
  local = `docker exec`/attach stdio, e2b = a raw-pty `pty-relay` bridged
  through the Node helper's stdio, ssh = ssh stdio. SSH (and tmux) may be
  the transport/host *under* the protocol on the ssh backend, but are
  never the cross-backend interface — same rule the proto applies to ssh
  ("transport … never the primitive").
- **The snapshot is ours, not the backend's.** E2B's PTY stream is a
  thin client over envd's `Connect` RPC and does not promise screen
  replay; even raw byte replay ≠ a reconstructed screen. So screen
  reconstruction is pillbox's layer. The snapshot is the PTY analogue of
  the structured channel's `SubscribeRequest.from_seq` replay: a bounded
  catch-up that lets a fresh renderer repaint *current* state.
- **Two front-ends, one pump.** The byte pump that reads frames is shared;
  it has a terminal front-end (writes to the user's tty — today's
  `pillbox run` / `session attach`) and a frame front-end (emits frames to
  an embedder). Same protocol, two renderers.
- **Reuse the control plane.** Lifecycle (`session.started/completed/
  failed`) stays on `events.jsonl` / `--events-webhook` / the `Event`
  vocabulary. This transport adds only the **data plane** (live screen
  I/O). Embedders correlate the two by session id.

## The session object

A *session* = (one sandbox) + (one agent under a PTY) + (a screen model),
addressed by the session id the `pillbox session` commands already mint.
Relationship to the proto: a proto `Run` is the headless/structured
execution of an agent; an attach session is the *interactive* execution
of one. Both bind to a `Sandbox`. Sessions are per-pillbox and not
inherited (unchanged from today).

## Wire protocol

Transport-agnostic: the bytes ride whatever pipe the backend hands us.
Framing follows orca's split — **binary length-prefixed frames for the
high-volume data plane, NDJSON for low-rate control** — because
full-screen repaints are large and base64-in-JSON would hurt.

Data-plane frame: `[type:u8][len:u32 BE][payload]`.

| Frame | Dir | Payload | Notes |
|---|---|---|---|
| `Hello` | C→H | cols:u16, rows:u16 | first frame on attach |
| `Snapshot` | H→C | ANSI bytes | exactly once, immediately after Hello |
| `Data` | H→C | raw PTY bytes | live output |
| `Input` | C→H | raw keystrokes | — |
| `Resize` | C→H | cols:u16, rows:u16 | SIGWINCH |
| `Signal` | C→H | signal name | e.g. detach/INT/TERM |
| `DataAck` | C→H | byte count:u64 | flow control (see below) |
| `Exit` | H→C | exit code:i32 | agent/PTY exited |

Control-plane (NDJSON, one object per line) carries lifecycle events
(reusing the `Event` schema) and out-of-band session state. Kept off the
binary channel so a consumer can subscribe durable-only.

### Snapshot recipe (validated)

The host builds the `Snapshot` payload from its `vt100` screen model as:

```
[ "\x1b[?1049h" if screen.alternate_screen() ]   // re-enter alt buffer
+ screen.state_formatted()                        // modes (see below)
+ screen.contents_formatted()                     // the grid
```

`vt100`'s `state_formatted()` carries the out-of-band modes orca had to
**hand-mirror** (mouse tracking + SGR encoding, bracketed paste,
application-cursor-keys, hidden cursor, title) — so our host is *simpler*
than orca's `@xterm/headless` + `SerializeAddon` + manual mode scanning.
Only the alt-screen *enter* is prepended manually (it's a buffer switch,
not grid state — orca ships it as the separate `isAlternateScreen` field).
The serialized ANSI is directly consumable by xterm.js: write the snapshot
string, then write live `Data`.

### Flow control

`Data` is **ephemeral** (mirrors `Event.ephemeral`): under backpressure
the host coalesces/drops live bytes rather than growing unbounded, and the
next `Snapshot` is the recovery. The client periodically sends `DataAck`
with a cumulative byte count; the host keeps a bounded outstanding window
per client (orca's `acknowledgeDataEvent`). A slow or stalled client never
OOMs the host. The screen model is always fed every byte regardless, so a
re-snapshot is always correct.

Orca's relay protocol (confirmed from its design notes) puts **seq + ack
in every frame header** (13-byte header: type, id/seq, ack, length) so
flow control and reconnect-resync are intrinsic, plus a 5 s **KeepAlive**
frame for liveness and a **version handshake** (mismatch = hard, non-retry
error). Our v1 header is leaner (`type + length`); phase 5 should adopt
seq/ack + a `Hello` version field rather than inventing a separate scheme.
Orca also keeps a **raw replay ring buffer (last ~100 KB)** *alongside*
the screen snapshot — the snapshot repaints the visible screen, the ring
buffer restores recent scrollback. Our `ScreenModel` is screen-only today;
the ring buffer is the scrollback upgrade.

## Rust surface (pillbox-core)

Library-first, matching the contract doc. New module `src/attach/`:

- `frame.rs` — the wire codec (encode/decode, no external deps).
- `screen.rs` — `ScreenModel`: a `vt100::Parser` wrapper exposing
  `feed(&[u8])` and `snapshot() -> Vec<u8>` (the recipe above).
- `mod.rs` — the contract traits + the shared pump.

```rust
/// Any bidirectional byte pipe a backend can open to a session's pty-host.
pub(crate) trait FramePipe: Read + Write + Send {}

/// A backend that can run an agent in a persistent, named PTY session and
/// re-open a pipe to it later. local_docker MAY opt out (host tty is the
/// PTY for the non-detached path); e2b/ssh implement it.
pub(crate) trait SessionBackend {
    /// Provision sandbox + launch the in-sandbox pty-host running the
    /// agent. Returns once the host is up; sandbox keeps running.
    fn launch(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox)
        -> Result<Session>;
    /// Open a frame pipe to an existing session's pty-host.
    fn attach(&self, session: &Session) -> Result<Box<dyn FramePipe>>;
    /// Tear down the sandbox.
    fn kill(&self, session: &Session) -> Result<()>;
}
```

This lifts today's e2b-only free functions (`reattach`, `kill_session` in
`sandbox/remote_e2b.rs`) into a cross-backend contract and adds `launch`.

### Backend mapping

| Concern | local docker | e2b | ssh VPS |
|---|---|---|---|
| pty-host location | host process (or in-container) | inside sandbox (`pillbox` mode) | on VPS (`pillbox` mode) |
| frame transport | `docker exec` stdio | raw-pty `pty-relay` via Node helper stdio | ssh stdio |
| snapshot | `ScreenModel` (vt100) | `ScreenModel` (vt100) | `ScreenModel` (vt100), or tmux |
| ssh-detach today | n/a | works | **falls out for free** |

## Validated by prototype (`/tmp/ptyproto`, 2026-05-27)

- **Fidelity:** a hard screen (alt-screen, 16/256/RGB color, bold, cursor
  parking, box-drawing) round-trips through `vt100` snapshot→reparse with
  identical text, cursor, attrs, **and modes** (mouse/bracketed-paste/etc.).
- **Wire shape:** a host owning a PTY + screen model served snapshot-then-
  stream over a unix socket; a second client attaching after the first
  detached got a snapshot reflecting *current* state (not blank, not a
  full replay), then resumed live streaming. PTY survived disconnects.

## Production requirements (not yet built)

1. **Flow control** — bounded per-client window + `DataAck`; coalesce/drop
   ephemeral `Data` under backpressure (prototype's broadcast is unbounded).
2. **Multi-client** — N simultaneous viewers per session (orca: desktop +
   mobile). Host structure supports it; untested concurrently.
3. **Real remote transport binding** — run the host *inside* an e2b/ssh
   sandbox with the pipe being a raw-pty `pty-relay`/ssh stdio (prototype proved
   only the local-socket case; the protocol is transport-agnostic by
   design but the binding is unproven). Local docker (phase 2b) is the
   first real binding: host inside the container, pipe = `docker exec`.
4. **Protocol hardening** (orca-confirmed) — seq/ack frame header for
   reconnect-resync, a `Hello` version handshake, KeepAlive frames, and a
   raw replay ring buffer for scrollback alongside the screen snapshot.

## Phasing

1. **Contract** — `frame.rs` + `screen.rs` (`ScreenModel`) + the traits,
   with unit tests (fidelity + frame round-trip). No backend wiring.
2. **Local backend** — pty-host subcommand + `docker exec` transport; route
   `pillbox run` (interactive) and `session attach` through the shared pump.
3. **e2b binding** — `e2b-helper.mjs` shrinks to a byte shuttle between
   host stdio and an in-sandbox raw-pty `pty-relay`; detach/handshake
   logic moves into the framed protocol (host pump owns Ctrl-A + SIGTERM).
4. **ssh binding** — same host over ssh stdio; ssh detach lands for free.
5. **Embedder front-end** — `pillbox session attach --protocol frames` +
   reference TS client; add flow control + multi-client.

## Non-goals

- **Not** a replacement for `agent.proto` — that's the semantic channel;
  this is the terminal channel. A consumer picks per surface (or uses both).
- **Not** a resident daemon. The host's lifetime is the session's.
- **Not** a replacement for `ssh user@host` as a human escape hatch.
