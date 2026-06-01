# opencode (and pi) as first-class, structured run targets — status

**Why:** opencode and pi are *structured-API-native*, which makes them better §0
citizens than claude/codex (which only emit a transcript JSONL you scrape).
opencode `serve` is a headless HTTP server with a typed `/event` SSE stream and
a prompt API — *literally* "an agent-as-a-service you put a frontend on." This
doc tracks wiring them in. See memory `pillbox-opencode-pi-structured-integration`.

## Done + verified (committed)

The hard core — opencode's event stream → pillbox's §0 — is built and tested:

- **`src/events/opencode.rs` — `EventMapper`** (commit `69a8dc4`): maps opencode
  `/event` envelopes `{type, properties}` → `contract::Payload`. Built + tested
  against a **real GLM turn** (not the OpenAPI schema — which was misleading; see
  below). Covers the `message.*` family:
  - `message.updated` `info:{id,role}` → `MessageStart` (first assistant id)
  - `message.part.delta` `{messageID,field,delta}` → `MessageDelta` (text) / `Thinking` (reasoning)
  - `message.part.updated` `part.type=tool` → `ToolCall{Running,Completed,Error}` (de-duped on mapped status)
  - `session.idle` → `MessageEnd` (open msg) + `AttentionRequired{NeedsInput}`
  - `permission.asked` → `Permission`; `question.asked` → `NeedsInput`; `session.error` → `ErrorStalled`
  - Ignores: text/reasoning part *snapshots* (deltas carry the content), `step-*`,
    `session.{updated,status,diff}`, the `session.next.*` family, `server.*`.
- **`drain_sse`** (commit `d7aa0bf`): reads an SSE stream (`data:` frames), maps
  each envelope, appends §0 events to the durable `SessionLog` — the same sink
  `session watch`/`subscribe` read. Transport-agnostic over a generic `Read`.
- **5 unit tests** (real-envelope fixtures) incl. an end-to-end raw-SSE → durable-log drain. No new deps.

**Live-verified against the runner image (`opencode 1.15.10`) with real GLM auth:**
`opencode serve` boots; created a session; drove a `glm-4.5-air` turn via
`prompt_async`; observed the real `/event` stream (`message.part.delta`×N,
`message.part.updated:tool`, `session.idle`) — which is what the mapper now
targets.

> ⚠️ **The schema lied; the wire didn't.** The OpenAPI advertises a
> `session.next.text.*` / `session.next.tool.*` family, and the first mapper was
> built on it. A real turn showed those emit only lifecycle bits
> (`agent.switched`, `model.switched`) — the **content streams over
> `message.*`**. Always verify opencode mappings against a captured turn, not `/doc`.

## Contract (verified live against `opencode 1.15.10`)

Use the **bare** routes — the `/api/*` namespace in `/doc` is GET-only/experimental
and POSTs there fall through to the SPA HTML.

- **Run:** `opencode serve --port <P> --hostname 127.0.0.1` (localhost-only;
  `OPENCODE_SERVER_PASSWORD` secures it — unneeded if curl runs in the same
  container).
- **Create:** `POST /session` with `{}` → `{"id":"ses_…", "slug", "projectID", …}` (top-level `id`).
- **Drive (streaming):** `POST /session/{id}/prompt_async`, body
  `{"parts":[{"type":"text","text":"<msg>"}],"model":{"providerID":"<prov>","modelID":"<model>"}}`
  → `204`, events stream on `/event`. (`POST /session/{id}/message` is the
  *synchronous* variant — returns the finished message, does **not** stream to `/event`.)
- **Read:** `GET /event` → SSE of `{id, type, properties}`.
- **Models:** `GET /api/model`. zai-coding-plan ids are GLM-5 era now
  (`glm-5.1`, `glm-4.7`, `glm-4.5-air`, …) — not `glm-4.6`.

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
4. **Session commands fork on integration** — `session send` (Server) → `docker
   exec <c> curl -X POST /session/{id}/prompt_async` with the parts+model body
   (not the pty-relay); `session watch`/`subscribe` already work (they read the
   log the bridge fills); `attach` has no PTY meaning for Server mode (decide:
   error with a pointer to `watch`, or attach = `watch`+`send`). The model
   (`providerID`/`modelID`) needs a source — pillbox default, `pillbox.toml`, or
   a `run` flag.
5. **pi** — sibling over `pi --mode rpc` / `--mode json` (structured stdio, not
   HTTP). Investigate its RPC schema first, then a stdio adapter feeding the same
   `SessionLog`. Different transport, same §0 sink.

## Auth: prefer device/API-key, not browser-loopback (verified)

opencode's **OpenAI** login is a PKCE **browser loopback** — the callback lands on
`localhost` *inside the sandbox*, which the host browser can't reach, so it
cancels (confirmed 2026-06-01). claude's loopback works only because pillbox
forwards its `oauth_port` (54545); opencode's `AgentSpec` has `oauth_port: None`,
so nothing's forwarded. Two fixes, in preference order:
1. **API-key / device-code providers** — e.g. **z.ai GLM** (API key) authed
   cleanly and is now in `~/.pillbox/global/auth/opencode` (`zai-coding-plan`).
   This is the recommended path for sandboxed agents.
2. Forward the agent's OAuth callback port (set `oauth_port`) for providers that
   insist on browser loopback.

**Live verify is unblocked** for GLM (auth present). For OpenAI specifically,
use a device/API-key path or wire the port-forward.

## Note

`opencode acp` (Agent Client Protocol) is another structured surface worth a look
— a standardized agent protocol that, if pillbox spoke it, could generalize the
"structured adapter" across ACP-supporting agents instead of per-agent code.
