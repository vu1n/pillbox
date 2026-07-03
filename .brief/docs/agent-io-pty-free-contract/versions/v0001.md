---
id: agent-io-pty-free-contract
project: pillbox
type: decision
status: active
title: The agent I/O contract is a PTY-free structured channel; pillbox is producer, not platform
related_code:
  - "src/contract.rs"
  - "src/events/opencode.rs"
  - "src/events/codex_serve.rs"
  - "src/agents/harness/**"
---

<!-- brief:anchor agent-io-pty-free-contract -->
## Normalize each agent's native structured protocol into one PTY-free Event vocabulary

The structured agent channel is **PTY-free**: pillbox runs each agent in its
headless/structured mode (claude `stream-json`+hooks, opencode `serve` HTTP+SSE,
codex `proto`/`exec`) and normalizes them into one `Event` vocabulary via
per-agent adapters. pillbox owns the *substrate* (sandbox + vault + workspace
lineage + the event emitter); it does **not** own threads/conversations,
identity/auth, chat surfaces, or fleet orchestration — every named consumer
already has those. Interactive PTY is deliberately *outside* this schema (it uses
the attach transport).

**Why.** pillbox is a producer, not a platform: the value is normalizing uneven
agent protocols into one vocabulary, degrading gracefully (a consumer always gets
lifecycle + final answer + result even when rich tool/phase events are absent) —
not re-owning the conversation/identity models consumers already have.

### Invariant
- Each agent adapter maps its native protocol to the shared `Event`/`Payload`
  vocabulary; coverage may be uneven but lifecycle + result must always survive.
- The structured channel is PTY-free; interactive PTY goes through the attach
  Frame protocol (`attach-frame-protocol`), never this schema.
- Durable vs ephemeral is explicit (`Event.ephemeral`): lifecycle/messages/results
  persist and replay; high-rate telemetry is best-effort and dropped first.
