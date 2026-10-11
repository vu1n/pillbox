# opencode as a first-class, structured run target — status

**Why:** opencode is *structured-API-native*, which makes it a better §0 citizen
than claude/codex in PTY mode (which only emit a transcript JSONL you scrape).
`opencode serve` is a headless HTTP server with a typed SSE event stream and a
prompt API — "an agent-as-a-service you put a frontend on."

## Status: OpenCode 2 on libkrun ✅

`opencode` is a first-class run target on the **libkrun** backend: `run` (→ a
*ready* session) / drive (`session send` → `POST /api/session/{id}/prompt`) /
read (`session watch`/`subscribe`) / teardown, any model provider (the standard
egress profile + `--egress-allow`). `AgentSpec.integration = Server`. The bridge
is backend-agnostic via the `SandboxHttp` seam (libkrun = HTTP/1.1 over a vsock
port-forward). `--model PROVIDER/MODEL`, default `zai-coding-plan/glm-4.5-air`.

The runner image pins **OpenCode 2.0.24** from the npm package `@opencode/cli`.
OpenCode 2 is a new package, not `opencode-ai@2` (that package stays on the 1.x
line), and its server API is a clean break from 1.x, so the adapter targets 2.x
only. The managed Cloudflare/DigitalOcean paths keep their own OpenCode 1.x pins
(`cloudflare-spike/Dockerfile`, `digitalocean/`) because they are sealed Huddles
execution cohorts; they are not covered by this doc. opencode does not run on
the deprecated Docker backend (`run` refuses every server agent there) and has
no `sandbox agent` adapter; the OpenCode 1 Docker paths were deleted.

**`run` does NOT auto-send** the initial prompt: it brings up a ready session; the
first prompt goes through `session send` like every turn, captured by the guest
event capture.

**§0 capture is complete + gateway-free**: the guest appends raw `/api/event` SSE
to a persistent file in the shared home; the host drains it (replay + follow via
`FollowReader`) on watch/subscribe, so a *late* watcher still gets the whole
history. OpenCode documents its live stream as volatile ("a slow consumer
overflows and fails the stream, and events during disconnection are missed"),
which is a second reason the capture lives next to the server rather than on
the host.

## Contract (verified live against OpenCode 2.0.24)

Captured with a free Zen model in an isolated home; the two captures are the
fixtures `src/events/fixtures/opencode-2.0.24-{tool-turn,failed-turn}.sse`.

- **Run:** `opencode serve --port 4096 --hostname 127.0.0.1`. OpenCode 2 always
  requires a server password (with none set it generates a random one), so the
  guest pins `OPENCODE_SERVER_PASSWORD` and every host call sends HTTP basic auth
  as user `opencode`. The password is a fixed, non-secret value: the server binds
  guest loopback and the host reaches it only through the session's private
  vsock socket. `opencode serve` without `--port` would start OpenCode's
  per-user background service on port 49374; pillbox never uses that mode.
- **Ready:** `GET /api/info` → `200 {"version":"2.0.24",…}`.
- **Create:** `POST /api/session` with `{"model":{"providerID":"<prov>","id":"<model>"}}`
  → `{"data":{"id":"ses_…",…}}`. The model is bound per session; prompts carry
  no model.
- **Drive:** `POST /api/session/{id}/prompt` with `{"text":"<msg>"}` → `200`
  with the admitted inbox item; the turn streams on `/api/event`. (There is no
  `prompt_async`; `POST /api/experimental/session/{id}/wait` blocks until idle.)
- **Read:** `GET /api/event` → SSE of `{id, created, type, location?, durable?, data}`.
- **Temperature:** there is no per-prompt temperature. `--temperature` is applied
  through `OPENCODE_CONFIG_CONTENT` as
  `{"providers":{"<prov>":{"models":{"<model>":{"body":{"temperature":T}}}}}}`,
  which merges over the built-in catalog. Checked against a capturing provider:
  that placement reaches the request body; agent `request.body` and the 1.x
  `temperature`/`options.temperature` fields are accepted but never sent.

### Event mapping (`src/events/opencode.rs`)

Each model step is its own assistant message (`assistantMessageID`).
Only the driven session's `session.*` events map: the first `sessionID` on the
stream (pillbox creates one session per fresh server), so a `task` subagent's
child session can't end the parent's turn. `permission.*`/`form.*` are not filtered.

| OpenCode 2 event | §0 |
|---|---|
| `session.text.started` / `.delta` (`.ended` only if no delta) | `MessageStart` / `MessageDelta` |
| `session.reasoning.delta` (`.ended` only if no delta) | `Thinking` |
| `session.tool.input.started {id,name}` | (names the call) |
| `session.tool.called {id,input}` | `ToolCall{Running}` |
| `session.tool.success {id,content}` / `.failed {id,error}` | `ToolCall{Completed}` / `ToolCall{Error}` |
| `session.step.ended` / `.failed` | `Usage` (native tokens + cost), `MessageEnd` |
| `session.execution.succeeded` | `AttentionRequired{NeedsInput}` (turn boundary) |
| `session.execution.failed` / `.interrupted` | `AttentionRequired{ErrorStalled}` |
| `permission.asked` / `form.created` | `AttentionRequired{Permission}` / `{NeedsInput}` |

OpenCode 2 declares `session.idle` and `session.status`, but neither appeared in
the captured turns; `session.execution.*` is the boundary. Always verify
mappings against a captured turn, not the OpenAPI document (that lesson from
1.x still holds: its OpenAPI advertised a `session.next.*` family that never
carried content).

## Auth

OpenCode 2 stores credentials in its SQLite store
(`~/.local/share/opencode/opencode.db`) and does **not** read OpenCode 1's
`auth.json`. A pillbox opencode home logged in under 1.x must log in again:
`pillbox auth login --agent opencode` (runs `opencode auth login --standalone`
in a sandbox). Provider keys in the environment (`--with`) are also picked up.
Browser-loopback OAuth providers still need the callback port forwarded
(`oauth_port` is `None` for opencode), so prefer API-key/device-code providers.

## Tool-free text turns (`pillbox.text/2`)

`pillbox text execute` drives one OpenCode turn in an invocation-owned microVM
(`src/execution/text_opencode.rs`, `src/sandbox/libkrun/repository/opencode.rs`;
the contract is in [commands](commands.md#sealed-local-text-execution)). The guest
driver starts `opencode serve`, creates one session, prompts it and relays each
`/api/event` object to the host as a JSON line until that session's
`session.execution.*` outcome. Checked against 2.0.24 with a capturing
OpenAI-compatible provider (fixtures `src/execution/fixtures/opencode-2.0.24-*`):

- A `{"action":"*","resource":"*","effect":"deny"}` rule, in config or on the
  session, removes every tool from the provider request (`tools` absent).
- If the model calls a tool anyway, OpenCode answers it with `No tool named …`
  (`session.tool.called` with `executed: false`) and asks the model again; it
  never runs it. The host stops the VM at the first `session.tool.*` event.
- A session created without a `title` makes a second, hidden title-generation
  call; its tokens show in `session.usage.updated` but in no step. Setting the
  title skips it.
- `tokens.input` excludes cache reads, and `tokens.reasoning` is reported beside
  `tokens.output` and priced as output.
- An unknown model variant fails the turn with `provider.no-route`
  ("Variant unavailable …"); variant names differ per model.
- Zen's free models refuse a session whose tools were removed (403
  `FreeTierError`, "OpenCode's free tier can only be used from within
  OpenCode"), so text turns reject them at `resolve`.
- The model catalog comes from `models.dev` at startup (an empty catalog when it
  is unreachable), so the text VM allows that host besides the provider's.
- Real GLM turns through the guest driver (outside a VM) completed on
  `zai-coding-plan/glm-5.3-flash` and `openrouter/z-ai/glm-4.5-air`, with the
  key supplied through the provider's environment variable (`ZHIPU_API_KEY`
  for both z.ai providers). Both answered in one step. OpenRouter reported a
  nonzero cost; models.dev prices the coding plan at zero. Fixtures:
  `opencode-2.0.24-{zai-coding-plan,openrouter}-turn.jsonl`.

## Not verified

- A full `pillbox run --agent opencode` inside a libkrun microVM with 2.0.24
  (needs macOS/HVF); the server contract and mapper were verified on the host
  against the same binary.
- Interactive `opencode auth login` inside the login sandbox.
- `permission.asked` and `form.created` on the wire (the default `build` agent
  allowed the captured tools without asking); their mapping follows the
  published 2.0.24 types.

## pi

pi is wired separately as a structured one-shot (`src/agents/harness/pi.rs`,
`pi -p --mode json`). A multi-turn `--mode rpc` driver is not built.
