---
id: attach-frame-protocol
project: pillbox
type: decision
status: active
title: One transport-agnostic Frame protocol carries the interactive PTY channel
related_code:
  - "src/attach/frame.rs"
  - "src/attach/screen.rs"
  - "src/attach/host.rs"
  - "src/attach/pump.rs"
---

<!-- brief:anchor attach-frame-protocol -->
## The Frame codec is the cross-backend PTY interface; the byte pipe is swappable

Interactive PTY attach rides one binary length-prefixed **Frame** protocol
(`src/attach/frame.rs`: `[type:u8][len:u32 BE][payload]`) over *any* bidirectional
byte pipe a backend supplies. The screen model (`screen.rs::ScreenModel`, a
`vt100::Parser` wrapper) reconstructs the current screen host-side so a fresh
renderer repaints *current* state (the PTY analogue of `Subscribe(from_seq)`
replay). The pump is shared; only the bottom byte-pipe changes per backend
(docker exec stdio, libkrun vsock).

**Why.** A backend should only have to supply a byte pipe; the frames on it are
identical everywhere, so the interactive channel is a local renderable object
across backends. The snapshot is pillbox's layer (the backend's raw byte replay
is not a reconstructed screen), which keeps late-join correct and the transport
genuinely swappable.

### Invariant
- The Frame codec + pump + screen model are transport-agnostic; a new backend
  adds a byte pipe, not a new frame scheme.
- Raw PTY bytes (`Frame::Data`) are **ephemeral** and never enter the durable
  session log — the log keeps periodic snapshots + the semantic stream.
- The screen model is fed every byte regardless of client backpressure, so a
  re-snapshot is always a correct recovery.

> **Built vs specced (backfill 2026-07-01):** shipped — the codec, `ScreenModel`,
> the host + pump, and both transports (docker exec + vsock). The proposed
> `FramePipe`/`SessionBackend` *traits* were **superseded**: lifecycle is
> per-backend free functions (`attach/mod.rs`), not a trait. **Not yet built**:
> the v1 header is `type+len` only (no seq/ack); `host.rs` broadcast is an
> unbounded `Vec<Sender>` — the `DataAck` frame exists but the bounded per-client
> flow-control window is unimplemented (a prerequisite before web fan-out).
