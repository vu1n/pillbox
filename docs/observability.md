# Observability

Pillbox emits lifecycle telemetry as OTLP without taking a dependency on
any specific backend. Point it at [Raindrop Workshop][workshop] for a
local-first debugger UI, at your team's collector for production-grade
storage, or at both — the wire format is the same.

For the command reference, see [../AGENTS.md](../AGENTS.md).

[workshop]: https://github.com/raindrop-ai/workshop

## Quick start with Workshop

[Workshop][workshop] is an MIT-licensed local trace debugger — daemon on
`localhost:5899`, SQLite at `~/.raindrop/raindrop_workshop.db`, no account
or external service required. It accepts standard OTLP, so pillbox plugs
in with one env var.

```sh
# 1) Install Workshop (one time)
curl -fsSL https://raindrop.sh/install | bash

# 2) Start the daemon (leave running)
raindrop workshop

# 3) Run pillbox with the OTLP endpoint pointed at Workshop
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:5899 \
  pillbox run --vault
```

Open `http://localhost:5899` — sessions show up as spans the moment they
complete.

## What pillbox emits

Three independent sinks, all driven by the same event stream:

| Sink     | Always on?                          | Carries                                            |
|----------|-------------------------------------|----------------------------------------------------|
| JSONL    | yes                                 | every event, appended to `<pillbox>/events.jsonl`  |
| Webhook  | when `$PILLBOX_EVENTS_WEBHOOK` set  | every event, POSTed as JSON                        |
| OTLP     | when `$OTEL_EXPORTER_OTLP_ENDPOINT` set | log record per event + sandbox-side terminal spans |

A failure in one sink never blocks the others — JSONL still lands even
if your collector is down.

### Lifecycle events

| Event              | Emitted by             | When                                        |
|--------------------|------------------------|---------------------------------------------|
| `session.started`  | host + sandbox         | Host: handshake. Sandbox: agent self-init.  |
| `session.completed`| sandbox                | Agent exited zero                           |
| `session.failed`   | sandbox                | Agent exited non-zero / errored             |
| `session.dropped`  | host                   | `pillbox session rm` torn the sandbox down  |

Both `session.started` lines share the same `session_id`; the `emitter`
attribute (`"host"` / `"sandbox"`) tells them apart. The delta between
them is sandbox cold-start latency.

### OTLP shape

- **Session spans:** sandbox-only, one per session, emitted at the
  terminal event. `trace_id` derives deterministically from the session
  id so host and sandbox views correlate without a lookup table. Span
  status maps `completed → Ok`, `failed → Error(reason)`.
- **Transcript spans (drain-mode):** one OTLP child span per
  agent-native transcript event, emitted by
  `pillbox session transcript <FILE> --session-id <ID> [--agent claude|codex]`.
  Harness auto-detected from path; `--agent` overrides.

  Supported harnesses:
  - **Claude Code** (`~/.claude/projects/<encoded>/<uuid>.jsonl`):
    `user` / `assistant` lines, content blocks (text, thinking,
    tool_use), `tool_result` blocks inside user content. Carries
    per-message model + usage + stop_reason on `assistant.text`
    spans.
  - **Codex** (`~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl`):
    `response_item` lines with payload types `message` (user /
    assistant), `function_call`, `function_call_output`,
    `reasoning`. `function_call_output` spans chain to their
    `function_call` via `parent_uuid` for future exact-chain
    visualization.

  Children parent the session span via shared `trace_id` so
  Workshop renders one trace per pillbox run with prompts, tool
  calls, and assistant turns as named child spans. Envelope-only
  events (Claude: mode/permission/attachment/file-history-snapshot;
  Codex: session_meta/turn_context/event_msg) are dropped.

  **Live tailing** is shipped: add `--follow` to either harness and
  pillbox blocks waiting on `notify` events, emitting spans as the
  agent harness appends new lines. Robust to partial-line writes
  (buffers the trailing fragment between FS events), file
  truncation (rewinds), and missing-file-at-start (idles until it
  exists). The bind-mount + auto-launch path that wires this into
  every sandbox automatically is the next layer.

- **`gen_ai` spans:** host-side, one per intercepted LLM API call
  (when `--vault` is on). Emitted from the vault MITM proxy with OTel
  GenAI semantic-convention attributes:
    - Envelope: `gen_ai.system`, `gen_ai.operation.name`,
      `server.address`, `http.request.method`, `url.path`,
      `http.response.status_code`, `pillbox.sandbox_id`
    - Orchestration: `pillbox.mode` (`"interactive"` /
      `"detached"`), `pillbox.workspace_id` (path-encoded pillbox
      key or `"global"`). Lets eval scoring stratify by
      attentiveness regime + group by project.
    - Body-derived (parsed from the SSE response stream as it passes
      through to the guest): `gen_ai.response.model`,
      `gen_ai.response.id`, `gen_ai.response.finish_reasons`,
      `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`,
      `gen_ai.usage.cache_read_input_tokens`,
      `gen_ai.usage.cache_creation_input_tokens`
  When the orchestrator threads a `session_id` through the vault,
  the gen_ai span's `trace_id` is derived from `session_id` and its
  `parent_span_id` from the session span — so all LLM calls nest
  under the session span as one trace per run. When `session_id`
  isn't available (sandbox-resident vaults, the ad-hoc `sidecar`
  command), the span falls back to a sandbox-id-rooted trace per
  lease.
- **Log records:** one per lifecycle event regardless of emitter.
  Severity matches the event type. Attributes mirror the JSONL field
  set 1:1 (`session_id`, `emitter`, `agent_id`, `backend`, `remote`,
  `label`, `status`, `exit_code`, `trace_path`, `result_snapshot`, …).
- **Resource:** `service.name = pillbox` by default, override with
  `OTEL_SERVICE_NAME`.
- **Transport:** OTLP/HTTP+protobuf via the blocking reqwest client.
  gRPC is opt-in behind the `otel-grpc` cargo feature.

### Limitations today

- **Body-derived attrs cover both SSE and non-streaming responses.**
  The SSE parser handles `stream: true` (Claude Code's default).
  When no SSE events are seen by end-of-stream, the parser falls
  back to a one-shot JSON parse of the accumulated body —
  populating the same `gen_ai.response.{model,id}` /
  `gen_ai.usage.*` / `gen_ai.response.finish_reasons` attrs from a
  non-streaming `/v1/messages` response. The raw-body buffer is
  dropped on the first successful SSE event so streaming responses
  don't pay the fallback's memory cost.
- **`gen_ai` span parenting kicks in for SSH + e2b remote runs.**
  The launcher mints `session_id`, bakes it into the VaultStdinBlob,
  and the sandbox-resident vault uses it to parent gen_ai spans
  under the session span. **Local-docker foreground runs still
  produce sandbox-id-rooted traces** — that path has no
  host-side session_id today (no wrapper, no session.* events), so
  there's nothing to parent under. Minting one for foreground runs
  is a small follow-up if and when local-docker grows a wrapper.
- **`gen_ai.request.model` not emitted.** We extract
  `gen_ai.response.model` from the SSE `message_start` event — the
  model the server actually served, which is usually what you want.
  The requested model (which may differ if a fallback fired) would
  require reading the request body and isn't on the roadmap unless a
  consumer asks for it.
- **Workshop accepts traces only** (`POST /v1/traces`). Pillbox will
  still try to ship log records to `/v1/logs`; Workshop returns 404
  and the log sink logs a warning. The spans land regardless. Use a
  real collector if you want logs and traces in one place.
- **Session spans require `PILLBOX_SESSION_STARTED_AT`** to be set
  by the in-sandbox wrapper — without it the span would have
  `start == end` and the sink skips emission. The session.* log
  records and the `gen_ai` spans still ship.

## Pointing at any OTLP collector

The endpoint env vars follow the OTLP spec exactly.

```sh
# Base URL; signal paths /v1/traces and /v1/logs are appended.
export OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.local:4318

# Override the full URL for one signal (skips the auto-append).
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://traces.example.com/v1/traces
export OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=https://logs.example.com/v1/logs

# Comma-separated k=v pairs added to every OTLP request.
export OTEL_EXPORTER_OTLP_HEADERS="authorization=Bearer abc,x-tenant=acme"

# Service name resource attribute. Defaults to "pillbox".
export OTEL_SERVICE_NAME=pillbox-prod

# Per-request timeout (default 2s). Standard OTLP env, honored by both sinks.
export OTEL_EXPORTER_OTLP_TIMEOUT=5

pillbox run --vault
```

Tested shapes:

- **Workshop** — `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:5899`
- **OTel Collector** — `…=http://collector:4318` (its default OTLP/HTTP
  port). Fan out from there to any backend the collector supports.
- **Honeycomb / Grafana Cloud / Tempo / Jaeger / Phoenix / Langfuse** —
  point at the vendor's OTLP/HTTP ingestion URL and set
  `OTEL_EXPORTER_OTLP_HEADERS` with the API key per their docs.
- **Raindrop SaaS** — Raindrop's hosted ingestion is *not* OTLP today
  (it uses Raindrop's own `/v1/events/track` API). Their local Workshop
  daemon is the OTLP-compatible surface. Run Workshop locally and let
  Workshop fan out to Raindrop SaaS if you want both.

## Running pillbox alone (no Workshop)

Workshop is convenient but not required. Two strictly-local alternatives:

- **JSONL only.** `<pillbox>/events.jsonl` always exists; tail it with
  `pillbox session events --follow` or `jq` directly.
- **Webhook sink to your own service.**

  ```sh
  pillbox run --vault --events-webhook http://localhost:9000/pillbox-events
  ```

  Each event is a single JSON POST; sandbox-side terminal events POST
  too (via the `PILLBOX_EVENTS_WEBHOOK` env the wrapper inherits).

## Plaintext warning

Pillbox warns once at startup when `OTEL_EXPORTER_OTLP_ENDPOINT` is
plaintext HTTP to a non-loopback host. Event attributes carry session
ids and any user-supplied `label` text — fine for in-cluster collectors
on a private network, sketchy for a cleartext public endpoint. Prefer
`https://` for remote collectors. Loopback (`127.0.0.1` /
`localhost`) over plain HTTP is silent.

## What's next

- **Bind-mount + auto-tail in the sandbox launchers.** The
  `--follow` tailer is the user-facing driver; wiring it into
  `pillbox run` (mount the sandbox's transcript dir to a host
  path, spawn a Tailer per session, tear down at sandbox exit)
  makes transcripts auto-stream without the user knowing the
  file path. Local-docker first; remote SSH/e2b need a
  sandbox-side relay. This is the last major chunk before the
  observability stack reaches "everything works automatically."
- **JSON-body fallback for non-streaming responses.** Mirror the SSE
  tap with a JSON-body parser for endpoints called with
  `stream: false` so usage attrs land uniformly.
- **Sandbox + orchestration context.** Resource / span attrs for
  `pillbox.mode`, `pillbox.concurrent_sandboxes`, `pillbox.fan_out`,
  `pillbox.workspace_id` — eval scoring stratified by attentiveness
  regime, and the wedge for multi-agent orchestration eval.
- **Hook-derived tool-call spans.** Optional per-harness layer that
  complements the universal MITM floor — picks up tool dispatch,
  subagent lifecycle, file edits, etc. Only available for harnesses
  that expose hooks (Claude Code yes; Codex no).
