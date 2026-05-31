# opencode (and pi) as first-class, structured run targets — status

**Why:** opencode and pi are *structured-API-native*, which makes them better §0
citizens than claude/codex (which only emit a transcript JSONL you scrape).
opencode `serve` is a headless HTTP server with a typed `/event` SSE stream and
a prompt API — *literally* "an agent-as-a-service you put a frontend on." This
doc tracks wiring them in. See memory `pillbox-opencode-pi-structured-integration`.

## Done + verified (committed)

The hard core — opencode's event stream → pillbox's §0 — is built and tested:

- **`src/events/opencode.rs` — `EventMapper`** (commit `55ff413`): maps opencode
  `/event` envelopes `{type, properties}` → `contract::Payload`. Covers the
  `session.next.*` streaming family + attention signals:
  - `session.next.text.{started,delta,ended}` → `MessageStart/Delta/End` (assistant)
  - `session.next.reasoning.delta` → `Thinking`
  - `session.next.tool.{called,success,failed}` → `ToolCall{Running,Completed,Error}`
  - `session.idle` / `question.asked` → `AttentionRequired{NeedsInput}`
  - `permission.asked` → `AttentionRequired{Permission}`; `session.error` → `ErrorStalled`
  - Stateful: synthesizes a per-turn `message_id` (text deltas carry only a
    sessionID); remembers `callID→tool name` for success/failed. Ignores the
    parallel `message.*`/`Part`/`sync.*` families so deltas aren't double-counted.
- **`drain_sse`** (commit `d7aa0bf`): reads an SSE stream (`data:` frames), maps
  each envelope, appends §0 events to the durable `SessionLog` — the same sink
  `session watch`/`subscribe` read. Transport-agnostic over a generic `Read`.
- **7 unit tests**, incl. an end-to-end raw-SSE-bytes → durable-log drain. No new deps.

**Live-confirmed against the real runner image (`opencode 1.15.10`):**
`opencode serve --port 4096 --hostname 127.0.0.1` boots; `GET /event` streams
(`{"type":"server.connected","properties":{}}` first frame). The read transport
is real.

## Contract (captured from the runner image's OpenAPI — `opencode serve` → `GET /doc`)

- **Run:** `opencode serve --port <P> --hostname 127.0.0.1` (localhost-only;
  `OPENCODE_SERVER_PASSWORD` secures it — unneeded if curl runs in the same
  container).
- **Read:** `GET /event` → SSE of `{id, type, properties}`. (Mapper handles it.)
- **Drive:** `POST /api/session/{id}/prompt`, body `{"prompt":{"text":"<msg>"}}`
  (only `prompt.text` required; optional `delivery`).
- **Create:** `POST /api/session` with `{}`.

⚠️ **Probe surprises to resolve in the morning (need a real session):** my quick
probe got an **empty `id` back from `POST /api/session`** (response shape differs
from the naive `.id` — likely nested, e.g. under `info`; confirm), and the prompt
POST then fell through to the SPA HTML because the session id was empty. So the
**create-response shape and the prompt round-trip are NOT yet verified** — don't
trust a create/drive helper until checked against a live session. The *read* side
(`/event` + the mapper) is the verified part.

⚠️ **Mapper caveat:** the `session.next.*` mappings are built from the OpenAPI
*schema*, not from *observed* events (observing them needs an AI turn → provider
auth). Field names/nesting should be re-checked against a real captured turn.

## Remaining wiring (mechanical, scoped)

1. **`AgentSpec` integration mode** — add `integration: Integration { Pty, Server }`;
   opencode → `Server`, claude/codex/pi → `Pty`. A const serve port.
2. **The event bridge** — `docker exec <c> curl -sN localhost:<P>/event` →
   `events::opencode::drain_sse` in a thread → host `SessionLog`. Mirror
   `remote_docker::spawn_transcript_stream` exactly (reuse `docker::exec_attach_at`
   + `TailerHandle::from_stream`); works for local *and* remote docker uniformly.
3. **Run path** (`local_docker::run`, guarded by `integration == Server`): launch
   `opencode serve` (not `pty-host`), spawn the bridge, record the session, print
   "server up — drive with `session send` / read with `session watch`". **Server
   mode has no PTY**, so this is a distinct path — claude/codex/pi keep the PTY
   path untouched. This is the invasive bit; do it guarded + run the full suite.
4. **Session commands fork on integration** — `session send` (Server) → POST
   `/prompt` (not the pty-relay); `session watch`/`subscribe` already work (they
   read the log the bridge fills); `attach` has no PTY meaning for Server mode
   (decide: error with a pointer to `watch`, or attach = `watch`+`send`).
5. **pi** — sibling over `pi --mode rpc` / `--mode json` (structured stdio, not
   HTTP). Investigate its RPC schema first, then a stdio adapter feeding the same
   `SessionLog`. Different transport, same §0 sink.

## The one morning step to unblock live verify

pillbox's auth dirs are **empty** — your opencode/pi logins are in your real home,
not pillbox's isolated auth. To let pillbox drive a real opencode turn:

```sh
pillbox auth login --agent opencode    # populates ~/.pillbox/global/auth/opencode
```

Then a real `serve` + prompt emits the `session.next.*` events, and we can
confirm the mapper against observed data + close out the create/drive shapes.

## Note

`opencode acp` (Agent Client Protocol) is another structured surface worth a look
— a standardized agent protocol that, if pillbox spoke it, could generalize the
"structured adapter" across ACP-supporting agents instead of per-agent code.
