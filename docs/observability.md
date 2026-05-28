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
- **`gen_ai` spans:** host-side, one per intercepted LLM API call
  (when `--vault` is on). Emitted from the vault MITM proxy with OTel
  GenAI semantic-convention attributes — `gen_ai.system`,
  `gen_ai.operation.name`, `server.address`, `http.request.method`,
  `url.path`, `http.response.status_code`, `pillbox.sandbox_id`. Calls
  within the same sandbox lease share a `trace_id` derived from the
  sandbox id, so Workshop and friends group them under one trace per
  agent run.
- **Log records:** one per lifecycle event regardless of emitter.
  Severity matches the event type. Attributes mirror the JSONL field
  set 1:1 (`session_id`, `emitter`, `agent_id`, `backend`, `remote`,
  `label`, `status`, `exit_code`, `trace_path`, `result_snapshot`, …).
- **Resource:** `service.name = pillbox` by default, override with
  `OTEL_SERVICE_NAME`.
- **Transport:** OTLP/HTTP+protobuf via the blocking reqwest client.
  gRPC is opt-in behind the `otel-grpc` cargo feature.

### Limitations today

- **`gen_ai.request.model` not yet emitted.** The first cut of MITM
  spans deliberately doesn't read request bodies, so model name is
  absent. Status, latency, error rate, and call counts per agent run
  are all there — token usage and model arrive in a follow-up that
  taps Anthropic's SSE stream for the `message_start` / `message_delta`
  usage events.
- **`gen_ai` spans aren't yet children of the session span.** The
  vault knows `sandbox_id`, not `session_id`. They land in their own
  trace per sandbox lease. Plumbing session_id into the vault is a
  separate, larger change.
- **Workshop accepts traces only** (`POST /v1/traces`). Pillbox will
  still try to ship log records to `/v1/logs`; Workshop returns 404 and
  the log sink logs a warning. The spans land regardless. Use a real
  collector if you want logs and traces in one place.
- **Session spans require `PILLBOX_SESSION_STARTED_AT`** to be set by
  the in-sandbox wrapper — without it the span would have
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

- **Token usage + model on `gen_ai` spans.** Tap the SSE response
  stream to extract `message_start.usage` (input tokens, cache stats)
  and `message_delta.usage` (output tokens). Adds the
  `gen_ai.usage.*_tokens` attributes Workshop's adapters expect and
  unblocks per-call cost dashboards.
- **Cross-trace correlation.** Plumb `session_id` through the vault
  so `gen_ai` spans become children of the session span instead of
  rooting their own traces.
- **Hook-derived tool-call spans.** Optional per-harness layer that
  complements the universal MITM floor — picks up tool dispatch,
  subagent lifecycle, file edits, etc. Only available for harnesses
  that expose hooks (Claude Code yes; Codex no).
