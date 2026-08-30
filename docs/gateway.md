# Session gateway boundary

Status: **local transport only** (updated 2026-08-31).

"Gateway" names two different responsibilities that must not be conflated:

- Pillbox's local gateway is a transient read/drive adapter over one local
  `SessionLog` and one runtime. It supports `session send`, `watch`, and
  `subscribe`; it is not a hosted collaboration service.
- Huddles owns the multiplayer gateway: participants, authenticated actor
  identity, ordering across participants, driver arbitration, retry/cancel
  intent, replay policy, and fan-out.

Managed Pillbox does not host the second responsibility. The former custom
Cloudflare `SessionGateway` Durable Object was removed under
`doc://pillbox/managed-tier-do-gateway@0002#managed-tier-do-gateway`.

## Local invariant

The durable spine is `sessions/<id>/log.jsonl`. `SessionLog::append` takes the
local append lock and assigns monotonic per-session sequence numbers. The
gateway process is ephemeral; no process is authoritative after it exits, and
the log resumes from disk on the next local reader/writer.

`session subscribe` is a localhost WebSocket view over that file-backed log.
It does not imply a remote Durable Object placement. Managed execution returns
bounded evidence to the caller, which appends it to the same local log.

## Ownership contract with Huddles

Huddles may call Pillbox's bounded execution API with a stable invocation and
idempotency key. Pillbox returns runtime evidence and result references. Huddles
then decides how those results participate in its collaborative order and
visibility model. Pillbox must not infer or persist Huddles participant state.

If a future request needs a hosted broker inside Pillbox, it is an architecture
change: propose a Brief amendment and satisfy
[durable-object-usage.md](./durable-object-usage.md). Do not revive the deleted
gateway, remote event sink/source, or per-event DO storage as an implementation
shortcut.
