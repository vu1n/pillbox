# Agent I/O contract — design notes

Status: draft (2026-05-26). Schema: [`proto/pillbox/v1/agent.proto`](../proto/pillbox/v1/agent.proto).

> Extended by [session-event-log.md](./session-event-log.md), which folds this
> `Event` vocabulary into a per-session durable log (adds a `sessionId`
> partition key, `actor`, `causationId`, a `Payload::Unknown` fallback). Kept
> separate — this is the proto-level contract consumers codegen against. Note:
> the "monotonic per pillbox" `seq` comment is **inaccurate** — `seq` is
> per-emitter (resets per run/exec).

The primitive: **a PTY-free, structured, bidirectional I/O channel to a
containerized agent.** pillbox runs the container; consumers
(orchestrators, Slack/chat, hermes, lum) send JSON and receive JSON and
never touch a terminal.

## Shape decisions

- **Producer, not platform.** pillbox owns the substrate — sandbox + vault +
  content-addressed workspace lineage + an in-sandbox event emitter. It does
  **not** own threads/conversations, identity/auth/multi-tenancy, chat
  surfaces, or fleet orchestration; every named consumer already has those.
- **Library-first, service-optional.** Ship `pillbox-core` + this schema.
  Consumers integrate by embedding (lum, Rust crate), subprocess/stdio
  (hermes), or webhook (a Slack bot). A networked `pillbox serve`
  (WS/gRPC/REST) is an optional later wrapper — and the only place auth lives.
- **Runs + lineage, not threads.** A `Run` is one agent execution; runs chain
  via `parent_run_id`. A consumer's "thread" = a run chain it assembles. This
  avoids conflicting with each consumer's own conversation model.
- **Two channels, one sandbox.** `agent` (talk to the agent) and `exec` (run
  one-off commands, e.g. `python foo.py`). Both PTY-free. Interactive PTY (a
  human drilling into a live shell) is deliberately *outside* this schema — it
  uses the existing attach transport, never the primitive (it bypasses the
  vault/workspace/audit envelope; structured `exec` doesn't).

## Why "no PTY" is real

Agents have headless modes, so the container runs the agent with **no TTY** —
its native structured protocol on pipes:

| Agent | PTY-free mechanism | Fidelity |
|---|---|---|
| Claude | `claude-stream` (libkrun): `claude -p --output-format stream-json --verbose --permission-mode auto --permission-prompts none` one-shot → §0 SessionLog. Hooks not wired. | messages, tool calls, result, usage, retries/denials as `Custom` |
| OpenCode | `opencode serve` (HTTP + SSE) | full |
| Codex | `codex-serve` (libkrun): `codex app-server` JSON-RPC | full |
| pi / Cursor | `pi --mode json` / `agent -p --output-format stream-json` one-shots (libkrun) | messages, tool calls, result |

**As built (code wins):** the interactive `claude` and `codex` agents stay PTY
(their §0 comes from transcript tailing). The structured siblings are separate
agent ids that share the PTY agent's auth home (`auth_id`): `codex-serve`
(server mode) and `claude-stream` (one-shot `Integration::Structured`, like pi
and cursor). A separate id rather than a flag because the integration kind is a
per-spec property that routes the run, the backend check (docker rejects
structured agents), and the capture/model policy.

`claude-stream` notes, verified against Claude Code 2.1.289 (the runner pin)
from captures of the real binary talking to a loopback API mock — no live
model run:

- **Credentials:** the run reuses the PTY path's vault env-fork. The guest
  mounts a cloned home whose `claudeAiOauth` tokens are stubs; the supervised
  VMM child's MITM swaps stub→real (reals reach it only on stdin). Egress is
  the same vault allowlist as the PTY `claude`.
- **Permissions:** the runner is root, and Claude refuses
  `--dangerously-skip-permissions` and `bypassPermissions` as root, so the run
  uses the PTY agent's `--permission-mode auto`. It also passes
  `--permission-prompts none`, so anything auto mode would escalate is denied
  (a `system/permission_denied` line, then the result's `permission_denials`)
  instead of stalling. `--bare` is not used because bare mode never reads
  OAuth credentials.
- **Mapping:** `system/init` starts the run and records the served model;
  `assistant` text blocks become message start/delta/end, and `tool_use` /
  `tool_result` become tool calls. `result` produces `RunFinished`: `is_error`
  decides failure, because `subtype` can be `success` on an API error. It also
  emits `Custom` `usage` (cost, turns, `terminal_reason`) and, when there were
  denials, `permission_denials`. `system/api_retry`, `system/permission_denied`
  and `rate_limit_event` become `Custom` events. `stream_event` (partial
  messages), `system/status` and `system/informational` are ignored. The
  capture is drained after the run, so partial deltas would only duplicate
  the complete lines.

pillbox's value is **normalizing these into one vocabulary** (the `Event`
oneof) via per-agent adapters that live *inside the sandbox emitter*.
Coverage is uneven, so the schema degrades gracefully: a consumer always gets
lifecycle + final answer + result snapshot even when rich `tool_call`/`phase`
events are absent.

## Cross-cutting

- **Durable vs ephemeral** (`Event.ephemeral`): lifecycle/messages/results
  persist and replay; high-rate telemetry (claude hooks, partial phases) is
  best-effort and dropped first under backpressure. Slack subscribes
  durable-only; a live UI opts into ephemeral.
- **Replay is free.** `Event.seq` + the existing `events.jsonl` let any
  consumer reconnect with `SubscribeRequest.from_seq` and catch up — no
  separate event store.
- **AG-UI is reference, not a dependency.** The event vocabulary borrows
  AG-UI's shape (message-stream / tool-call / lifecycle) where it's good, and
  diverges where pillbox's domain needs it (explicit `PhaseChanged`/
  `TodosUpdated` over generic JSON-Patch state; sandbox/workspace events
  AG-UI lacks). No formal AG-UI/ACP adapter unless a consumer ever demands it.

## Open call: permission scope

`PermissionMode` + `PermissionRequested`/`ResolvePermission` are in the schema
so it's forward-compatible, but the **recommendation for v1 is `AUTO_ALLOW`**
(headless, fire-and-collect-result) — `INTERACTIVE` ("approve-from-Slack")
needs in-sandbox permission-prompt routing (a permission-prompt tool / SDK
callback) that is a phase-2 build, not free. Flip to `INTERACTIVE` when that
routing lands; no schema rework required.

## What an emitter must do

The contract from the emitter's side — what a per-agent adapter (or anything
that wants to speak `agent.proto` to pillbox's consumers) has to honor. Read
top to bottom; the checklist at the end is the same list compressed.

**Speak the vocabulary, not the harness.** Emit the `Event` oneof —
lifecycle (`RunStarted` / `RunFinished` / `RunFailed`), output (`MessageStart`
/ `MessageDelta` / `MessageEnd` / `ToolCall` / `Thinking` / `Usage`),
human-in-the-loop (`PermissionRequested` / `AttentionRequired`), workspace
(`Checkpoint` / `ResultReady`) — never the harness's own frames. Harness-specific
data that has no arm goes in `Custom`, not in a new arm.

**Never a TTY.** Run the agent headless on its native structured protocol
(stream-json, `opencode serve`, `codex proto`). A PTY bypasses the
vault/workspace/audit envelope; the interactive attach transport is a separate
surface and is not this contract.

**Degrade to lifecycle, not to nothing.** Coverage is uneven across harnesses.
Whatever else is missing, a consumer always gets `RunStarted`, a final answer,
and `RunFinished { result_snapshot }` or `RunFailed { reason, exit_code }`.

**Mark durability honestly.** `ephemeral = true` is for high-rate telemetry
(`PhaseChanged`, `TodosUpdated`, partial phases) that may be dropped under
backpressure. Everything a consumer would replay — lifecycle, messages, tool
calls, results — is durable.

**Number durable events, and only those.** `seq` is monotonic per emitter
(per run / per exec) on durable events; ephemeral events carry `seq = 0`. A
consumer reconnects with `SubscribeRequest.from_seq` and expects to catch up
without a separate store, so a gap or a reused `seq` breaks replay.

**Signal the turn boundary.** When the agent ends a turn and is waiting to be
driven, emit `AttentionRequired { reason: NeedsInput }`. This is what
`session wait-idle`, the `needs-input` status, and the `AwaitingInput`
condition are derived from; an adapter that forgets it leaves every driver
polling.

**Correlate.** `ToolCall` lands twice (Running, then its result) under one
`tool_call_id`; `Usage` names its `message_id`; `RunStarted.parent_run_id`
carries lineage. Consumers count invocations and stitch trees from these ids,
not from ordering.

**Ship the result as a handle.** The agent's output workspace is a snapshot
handle on `RunFinished` / `ResultReady`, not a path and not inlined bytes.

### Checklist

- Emits the `Event` oneof; harness extras go in `Custom`
- Runs the agent with no TTY, on its structured protocol
- Always produces `RunStarted` and a terminal `RunFinished` / `RunFailed`
- Sets `ephemeral` on telemetry; durable on everything replayable
- `seq` monotonic per emitter on durable events, `0` on ephemeral, no gaps or reuse
- Emits `AttentionRequired { NeedsInput }` at every turn boundary
- Correlates tool results, usage, and lineage by id
- Reports the result workspace as a snapshot handle

## Suggested phasing

1. **claude (stream-json + hooks)** end-to-end: `Spawn` → events → `ResultReady`;
   `exec` channel; ship via in-proc callback + webhook + stdio. `AUTO_ALLOW`.
2. **opencode serve** adapter (lum already has it); `SendInput` follow-ups.
3. **codex proto** (lifecycle-first); interactive permission routing.
4. **`pillbox serve`** (WS/gRPC/REST + auth) — only if a network consumer needs it.
