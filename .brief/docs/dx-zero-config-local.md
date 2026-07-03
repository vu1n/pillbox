---
id: dx-zero-config-local
project: pillbox
type: decision
status: active
title: Every load-bearing journey has a zero-config local path; collectors are opt-in graduation
related_code:
  - "src/commands/session/**"
  - "src/events/log.rs"
  - "src/gateway.rs"
---

<!-- brief:anchor dx-zero-config-local -->
## Expose the streams locally and zero-config; a first-party reader is only a reference consumer

Every load-bearing journey must have a zero-config **local** path. OTLP
collectors, managed backends, and team infra are opt-in *graduation*, never
prerequisites. The substrate exposes its streams locally (the PTY *and* the
structured `Subscribe(from_seq)` event stream) so any consumer subscribes without
standing up a collector. A first-party `pillbox watch` is allowed **only** as a
reference consumer over the same public `Subscribe` contract everyone else uses —
never a privileged path (the `docker logs` / `git log` model).

**Why.** The integrated bundle is the product, and its DX is the deliverable — so
"slow-and-blind by default" on the journeys the docs sell hardest is a product
failure, and a privileged first-party reader would be exactly the lock-in the
local-first identity is avoiding. Local == public parity, or it's lock-in.

### Invariant
- The structured event stream is available locally + zero-config (JSONL under the
  state dir + a local `Subscribe` WS), not OTLP-only.
- `pillbox watch`/`subscribe`/`list`/`diagnose` read the same public log surface
  a third-party consumer would; no reader gets a privileged internal path.
- pillbox exposes streams; consumers (lum, Slack, an IDE) render — this is not a
  license to grow a UI.
