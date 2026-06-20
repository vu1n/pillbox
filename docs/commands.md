# Command reference

The full `pillbox` CLI surface — the detail behind the AGENTS.md router.

> **NOTE:** docker-backend commands/flags below are **deprecated** — libkrun
> is THE backend ([decisions.md](./decisions.md) ADR-001/002). They remain
> documented until the docker backend is deleted.

## Command surface

### Lifecycle (top-level)

| Command | What it does |
|---|---|
| `pillbox init` | Create the global pillbox. Idempotent. |
| `pillbox new [--name N] [--agent A] [workspace flags]` | Create a project pillbox in cwd. Writes `pillbox.toml` + state dir + initializes a rustic repo. |
| `pillbox list [--json]` | Every pillbox on disk (global + projects). |
| `pillbox rm NAME` | Delete a project pillbox. Refuses `global`. |
| `pillbox info [--json]` | Show the resolved pillbox for cwd (or `--pillbox`). |

### Per-pillbox

Every command below resolves the current pillbox from cwd (or
`--pillbox NAME`). Add `--global` to writes that should target the
global pillbox regardless of where you are.

| Command | What it does |
|---|---|
| `pillbox run [--agent A] [opts] [-- args]` | Launch the agent against the current pillbox. |
| `pillbox dispatch --from-bookmark NAME -k N (--rubric FILE \| --cmd "VERIFIER") [--segments SPEC] [--retries N] [--agent A] [--model M] [--temperature F] [--memory] [--ttl DURATION] [--json] -- PROMPT…` | Fork `k` detached worker sessions from a snapshot bookmark onto one prompt, drive each to idle, **grade** each (rubric/cmd), **retry** failures with distilled feedback, **select** the highest-scoring passer (tie-break: fewer retries, then earliest), and **pull** its workspace. Emits a `DispatchVerdict` (`docs/dispatch.md`); exit 0 = winner, 1 = no worker passed, 2 = usage. **libkrun-only today** (grader resolves the live workspace a libkrun-only way; docker deferred). Each worker's evidence lands as a `dispatch.worker_summary` §0 artifact. Losers are left running for `session rm`/`prune` (not auto-killed) — pass `--ttl` + `session prune` to reap a campaign. **Two axes:** fork-`k` = best-of-k **diversity** (use `--temperature`); **`--segments SPEC`** = the proven in-session **segmentation** lever — drive an ordered TOML chain of focused, checkpoint-gated sub-prompts in ONE session (the `--rubric`/`--cmd` stays the final reward; gates are per-segment). They compose. |
| `pillbox secret add NAME [opts]` | Store a secret. Scope: resolved (use `--global` to force global). |
| `pillbox secret list [--json]` | List secrets visible from the current pillbox (project + global, deduplicated). |
| `pillbox secret show NAME [--reveal] [--to-stdout] [--json]` | Show one secret (inheritance applies). |
| `pillbox secret rm NAME [--global]` | Delete a secret. |
| `pillbox env load NAME PATH [--global]` | Parse `.env` file, store as bundle. |
| `pillbox env list/show/rm` | Same shape as secrets. |
| `pillbox auth login --agent A` | Run the agent's OAuth flow inside a sandbox. Always writes to global. |
| `pillbox auth list/rm` | List/remove agent OAuth state. |
| `pillbox vault ca/status [--json]` | Inspect the per-pillbox vault CA. |
| `pillbox sidecar [--bind] [--json]` | Standalone vault sidecar process. |
| `pillbox session list [--json]` | List sessions started from this pillbox (oldest first). |
| `pillbox session info ID [--json]` | Show one session (accepts unique id prefix ≥ 4 chars). |
| `pillbox session diagnose ID [--json]` | Diagnose one session: derived status, failure detail, and an activity summary from the durable log — the "what happened / why is it stuck" companion to `info`. Accepts an id prefix ≥ 4 chars. |
| `pillbox session attach ID` | Reattach to a detached session. Detach again with Ctrl-A + D or `pillbox session detach ID` from another shell. Works for the local Docker and libkrun backends. |
| `pillbox session detach ID` | Signal a currently-attached pillbox to detach (SIGTERM, no-op if already detached). |
| `pillbox session send ID TEXT` | Drive a running (detached) session: push TEXT to the agent's PTY as if typed — the programmatic SendInput half (pair with `session subscribe` to read the response). Bytes sent as-is; add a trailing newline/`\r` to submit a prompt to a TUI agent. Local Docker sessions today. |
| `pillbox session annotate ID TEXT [--anchor REF]` | Record an attributed, durable §0 comment WITHOUT driving the agent — the async, keyboard-free "chime in" (distinct from `send`, which steers). Lands in the log stamped with your actor; an orchestrator may inject it as agent context. `--anchor` references what it's about (a seq, a path, a message id). |
| `pillbox session subscribe ID [--from SEQ] [--bind ADDR]` | Stream a session's durable event log to WebSocket subscribers as JSON (one Event per text frame, in seq order from `--from`). For a **live** (detached) session it also tails the transcript→log while serving, so a driven detached session is readable; for a foreground/historical session it serves the existing log. Binds localhost (`--bind`, default `127.0.0.1:0` — printed) until Ctrl-C. The §0 local read surface a chat bridge / orchestrator / browser connects to without a shell. If `$PILLBOX_EVENTS_WEBHOOK` is set, also POSTs attention signals to it (read-side). |
| `pillbox session watch ID [--from SEQ]` | Render a session's event stream to **this terminal** — messages by role, tools (⚙/✓/✗), thinking, the ⏳ attention signal — the human-facing reader (`docker logs` model; `subscribe` is the machine/WS sibling). Tails a live session as it works. Ctrl-C to stop. Accepts an id prefix. |
| `pillbox session rm ID` | Tear down the backend (kill sandbox) and remove the session record. |
| `pillbox session done ID --status ok\|failed [--reason TEXT] [--exit-code N] [--trace-path PATH] [--result-snapshot HANDLE]` | Emit `session.completed` / `session.failed` to every configured sink. Invoked automatically by the in-sandbox wrapper after the agent exits (also passes `--result-snapshot` from the result workspace push); can also be called manually. Does NOT tear down the sandbox — use `session rm` for that. |
| `pillbox session pull ID [--to DIR]` | Rehydrate a session's result workspace into a directory. Reads `result_snapshot` from the session record; errors clearly if the agent hasn't finished. Default `DIR` is `./session-<id>`. |
| `pillbox session score ID (--cmd "VERIFIER" \| --rubric FILE) [--snapshot HANDLE \| --workspace DIR] [--in-sandbox] [--grader-egress HOST]… [--json]` | **Externally grade** a session's result — the verifiable, non-self-reported reward channel (vs `session done --status`, which is the agent's self-report, Goodhart-banned). `--cmd VERIFIER` runs one command via `sh -c` with cwd = the rehydrated `result_snapshot` (or `--snapshot`/`--workspace`), captures its **exit code + output**, and appends a `scored` §0 event (exit 0 → passed/score 1.0, else 0.0; combined output → `feedback`, tail-capped at 32K). **`--rubric FILE`** (mutually exclusive with `--cmd`) grades against a checklist — each non-blank, non-`#` line is `NAME :: COMMAND`, a named criterion run in the same workspace — and the `scored` event gains **per-criterion verdicts** (`criteria: [{name, passed, feedback}]`) with `score` = the passed fraction (a real gradient; the decomposed feedback an optimizer reflects on). Default runs on the host; **`--in-sandbox`** runs it in a one-shot microVM (the runner toolchain, offline + secret-free) — for real repos whose tests need the image's deps (libkrun feature). **`--grader-egress HOST`** (repeatable, in-sandbox only) opens the grader's DNS-fence to the listed hosts so its tests can fetch deps (`--grader-egress pypi.org --grader-egress files.pythonhosted.org` for pip; `registry.npmjs.org` for npm) — same MITM-with-empty-swap path as a vault run, no creds, every other host fenced; trades offline reproducibility for reachability. **`--json`** emits the verdict (`{version, session, grader, passed, score, feedback, seq}`) on stdout so a loop reads the result directly — no stdout-scrape, no §0-log reach-in; `seq` is the `scored` event's log seq. Otherwise read back via the session's §0 log (`session subscribe`/`watch` or the log file); the optimization loops consume these. |
| `pillbox session ingest ID [--json]` | Drain a session's durable raw §0 capture (its persisted `/event` stream) into the canonical `log.jsonl`, **post-hoc and idempotent**. For headless/batch runs (the optimization loop) where no live `subscribe`/`watch` filled the log: the reparented guest outlives `run`, so a host-side live tailer can't persist for it, but the guest's capture file does — so the full agent **trajectory** (messages + tool calls) lands in the §0 log without racing the session. Run it BEFORE `session score` so trajectory events precede the `scored` event in seq order. Re-running is a no-op (`.ingested` marker). **libkrun opencode today**; docker/PTY sessions drain live via `session subscribe`/`watch`. No VM boot — pure file read + log append. |
| `pillbox session log ID [--type TYPE]… [--last] [--from SEQ]` | **Read** a session's durable §0 log (`log.jsonl`) — the per-session event stream `score`/`ingest`/`wait-idle` write — as one event JSON per line in seq order. The structured §0 read an orchestrator/eval harness uses instead of opening the on-disk log by hand. `--type` filters by payload tag (repeatable; snake_case, e.g. `tool_call`, `scored`, `message_end`; an unknown tag matches nothing — typo → empty, not error); `--last` keeps only the final match (the "latest scored/idle verdict" read); `--from SEQ` starts mid-log. Resolves the session **record** (consistent with `score`/`ingest`), so an already-`rm`'d session's orphaned log isn't readable this way. Distinct from `session events`, the pillbox-wide *lifecycle* stream. |
| `pillbox session wait-idle ID [--timeout SECS] [--from SEQ]` | Block until the session's current turn goes **idle** — the agent finished and is waiting for input (the §0 `AttentionRequired` signal) or the run terminated (`RunFinished`/`RunFailed`). The drive-surface "turn done" primitive: `session send` a prompt, then `wait-idle` instead of polling. Drains the §0 capture into the durable log **while** waiting (so the trajectory lands too — a later `session ingest` is then redundant). Exits 0 on idle, **1 on `--timeout`**. Waits for an idle event after the current log tail (or `--from SEQ`). `session info --json` exposes the result-workspace path for graders/orchestrators. |
| `pillbox session artifact put ID --kind K [--summary S] [--content-type T] [--class content\|signal] [--worker WID] [--file PATH] [--json]` | Attach a **structured artifact** to a session — a typed, durable output that isn't an ordinary agent message (a grader report, judge critique, dispatch worker summary, code-exploration citations, self-harness proposal, patch metadata). Reads the body from `--file` or stdin, stores it in the session's **content-addressed blob store** (`sessions/<id>/blobs/<sha256>`, idempotent dedup), and appends an `artifact` §0 event holding the small typed reference (`kind`/`summary`/`contentType`/`class`/`blobRef`/`bytes`/`workerId`) — the body **never inlines** into the log, so a big payload can't drown replay. `--kind` is a free-form dotted namespace (`eval.grader_report`, `judge.report`, `dispatch.worker_summary`, `code_explore.citations`, …); readers filter by prefix. `--class` carries the content-vs-signal poolability split (default `content` = local-only; `signal` = scrub-poolable metadata). `--json` emits `{version, session, seq, kind, ref, bytes, class}` so a loop reads the ref directly. The generalization of `scored` — the way any host-side tool (a grader, a judge, a FastContext explorer, the dispatch loop) attaches output without inlining it or being compiled into pillbox. The `artifact` event is the foundation the eval loop + dispatch-evidence channel consume. |
| `pillbox session artifact get ID --ref SHA256` | **Read** an artifact body by its blob ref (the lazy dereference of a `blobRef` seen via `session log --type artifact`) — streams the bytes to stdout. The ref is validated as a bare sha256 handle before any filesystem touch (the path-traversal guard). List a session's artifacts with `session log --type artifact`. |
| `pillbox session prune [--dry-run]` | Tear down every session whose `expires_at` is in the past (calls `session rm` per record). Sessions without `--ttl` are left alone. Intended for cron/orchestrator schedules; pillbox doesn't auto-prune. |
| `pillbox session events [--follow] [--json]` | Tail the local events stream (`<pillbox>/events.jsonl`). |
| `pillbox session transcript FILE --session-id ID [--agent claude\|codex] [--follow]` | Drain an agent-native transcript (Claude Code `~/.claude/projects/<encoded>/<uuid>.jsonl` or Codex `~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl`) and emit one OTLP child span per rendered event, parented under the session span derived from `ID`. Harness auto-detected from path; `--agent` overrides. `--follow` drains then blocks waiting on FS notifications and emits spans for each appended line (Ctrl-C to stop) — the "watch your agent think" mode. Requires `OTEL_EXPORTER_OTLP_ENDPOINT` to actually ship; parser runs regardless and reports the event count. See [docs/observability.md](./docs/observability.md). |
| `pillbox doctor [--json]` | Diagnose Docker, image, perms, `$HOME`. |
| `pillbox version` | Print pillbox + runner image versions. |
| `pillbox push [--tag T] [--message M] [--bookmark NAME] [--parent HANDLE]… [--json]` | Snapshot cwd into the pillbox's rustic repo. `--bookmark NAME` also points a bookmark at the new snapshot atomically (snapshot+name in one call, bound to this push — no handle-copy or `latest` race; needs a project pillbox). `--parent HANDLE` (repeatable, prefix-ok) records parent snapshots as this one's **lineage** — the merge-back edge an orchestrator declares after merging collected results (`push --parent <base> --parent <winner>`). Parents resolve to full ids (unknown parent → error) and are stored pillbox-native (survive `session rm`, work on S3/R2); surfaced as a `parents` array by `snapshot show/list --json`. |
| `pillbox pull [--snapshot HANDLE \| --bookmark NAME]` | Restore cwd from a snapshot (defaults to latest) or bookmark. |
| `pillbox collect SESSION… [--to DIR] [--as-refs] [--json]` | **Collect** finished session results + lineage for an orchestrator to merge — the substrate half of a fan-out loop (pillbox collects, the orchestrator decides how to merge; `dispatch` = `collect` + grade + select-one). Rehydrates each session's result tree into `<to>/<session>/` (default `./collected`) and emits a `--json` manifest of the **merge triple** per result: `base_snapshot`/`base_git_anchor` (fork point + merge base commit), `result_snapshot` (theirs), `dir`, `source`. All-or-nothing on unfinished sessions. `--as-refs` also synthesizes a git commit per result (tree = result, parent = merge base) under `refs/pillbox/collect/<session>` so the orchestrator `git merge`/`jj`s with its own policy (requires cwd = git work tree; adds `ref` to the manifest). pillbox never merges. See docs/collect.md. |
| `pillbox snapshot list [--json]` | List every snapshot in the pillbox's repo. |
| `pillbox snapshot show HANDLE [--json]` | Show one snapshot (HANDLE may be a unique prefix). |
| `pillbox snapshot rm HANDLE` | Forget a snapshot (data packs survive until prune). |
| `pillbox bookmark list [--json]` | List named snapshot bookmarks. |
| `pillbox bookmark show NAME [--json]` | Show one bookmark. |
| `pillbox bookmark set NAME [HANDLE\|latest]` | Point a bookmark at a snapshot (defaults to latest). |
| `pillbox bookmark rm NAME` | Remove a bookmark; the underlying snapshot is untouched. |
| `pillbox workspace rekey` | Rotate the rustic repo password. **Caveat:** rustic_core 0.11 has no public API to delete the prior key; both passwords keep working until upstream lands deletion. Treat the old password as compromised. |

### Sandboxes — a long-lived exec target (Docker)

`pillbox run` launches one agent turn. The `sandbox` group instead spawns a
**long-lived** sandbox you keep around and `exec`/`agent` into repeatedly — the
PTY-free exec channel an orchestrator drives. Docker-backed today.

| Command | What it does |
|---|---|
| `pillbox sandbox spawn [--image IMG] [--agent A] [--workspace PATH] [--label TEXT]` | Spawn an idle sandbox with the workspace mounted; prints the sandbox id. `--agent` provisions its auth + runs non-root so the agent channel can drive it; omit for a bare exec-only sandbox. |
| `pillbox sandbox exec ID [--json] -- ARGV…` | Run a command (PTY-free). Streams raw output + mirrors the exit code; `--json` emits `ExecStarted`/`ExecOutput`/`ExecExit` as JSONL. |
| `pillbox sandbox agent ID [--json] -- PROMPT…` | Run an agent turn (the agent channel) in a sandbox spawned `--agent`. Streams contract events; `--json` for JSONL, else a human trace. |
| `pillbox sandbox list [--json]` | List sandboxes in the current pillbox. |
| `pillbox sandbox destroy ID` | Kill the sandbox container and remove the record. |

### `pillbox run` flags

| Flag | Default | Purpose |
|---|---|---|
| `--agent A` | `pillbox.toml` `agent` field, then `claude` | Agent to launch (`claude` \| `codex` \| `codex-serve` \| `opencode` \| `pi`). `codex-serve` drives `codex app-server` (codex's structured JSON-RPC protocol) as a server-mode agent — libkrun-only, shares `codex`'s auth (one `auth login --agent codex`), driven via `session send` + read via `session watch`/`subscribe`. The PTY `codex` is the default and unaffected. |
| `--workspace PATH` | cwd | Host directory to mount. |
| `--name NAME` | `pillbox.toml` `name`, else basename(workspace) | Mount-point name (`/workspace/NAME`). |
| `--mount HOST:GUEST` | — | Extra bind mount. Repeatable. |
| `--with NAME[=ENV_VAR]` | — | Inject one stored secret. |
| `--env BUNDLE` | — | Inject every variable from a stored env bundle. |
| `--env-file PATH` | — | Inject every variable from a `.env` on disk. |
| `--vault` | — | Route agent traffic through the stub-swap proxy. |
| `--egress-allow HOST` | — | Open one host through the libkrun egress fence **and** the vault broker allowlist (repeatable; exact match, or `.suffix` for subdomains). For a self-hosted model endpoint or a registry a build needs. |
| `--egress-deny` | — | Switch on the vault broker's **default-deny**: block outbound to any host with no credential provider that isn't on `--egress-allow`. Enforced at the vault proxy — needs `--vault` (or a vaulted `--with`), else warns and is a no-op. See [docs/vault.md](./docs/vault.md). |
| `--memory` | — | Wire in swarm memory (the external [`kypp`](https://github.com/vu1n/kypp) engine, attached not owned): brief the agent from project memory at start, capture this session's §0 log after. Host-side, best-effort — a missing/erroring `kypp` warns, never fails the run. |
| `--mcp NAME=URL` | — | Attach a shared MCP server (`http(s)://`). NAME is what the agent sees; `localhost`/`127.0.0.1` are rewritten to `host.docker.internal`. Repeatable. See [docs/shared-mcp.md](./docs/shared-mcp.md). |
| `--mcp-token NAME=SECRET_NAME` | — | Attach a bearer token (from the secret store) to a `--mcp NAME`. claude folds it into a 0600 headers tempfile; codex into an env var via `bearer_token_env_var`. Never lands in argv or shell history. Repeatable. |
| `--from-bookmark NAME` | — | Start from a named snapshot bookmark — restore that bookmark into the workspace before launching the agent. |
| `--detach` | — | Start the session and immediately return — the agent keeps running in the background; reattach with `pillbox session attach <id>`. Works for both local backends (Docker and libkrun). Local `--detach` does NOT support `--vault` (the host-side proxy can't outlive the CLI). |
| `--events-webhook URL` | — | POST every lifecycle event to URL as JSON. Forwarded to the in-sandbox wrapper so terminal events (`session.completed`/`failed`) reach back to the orchestrator. Equivalent to `$PILLBOX_EVENTS_WEBHOOK`. See [docs/observability.md](./docs/observability.md) for the full sink reference (JSONL / webhook / OTLP via `$OTEL_EXPORTER_OTLP_ENDPOINT`). |
| `--ttl DURATION` | — | Per-session retention TTL — `30m` / `24h` / `7d` (`s`/`m`/`h`/`d` units only, max 365d). Writes `expires_at` to the record. `pillbox session prune` drops expired sessions. Requires `--detach`. |
| `--label TEXT` | — | Human label for a detached session, surfaced in `pillbox session list`. Only meaningful with `--detach`. |
| `--json` | — | Emit the started session as `{version:1, session:{id,…}}` on stdout instead of the human banner — `pillbox run --json \| jq -r .session.id`. Needs a persisted session: a `--detach` run (any agent) or a server-mode agent (`opencode`, always reparented). A foreground PTY run has nothing to emit and is rejected at dispatch. |
| `--model PROVIDER/MODEL` | agent default | Model for a server-mode agent (`opencode`), e.g. `zai-coding-plan/glm-4.5-air`. Ignored by PTY agents. |
| `--temperature FLOAT` | — | Sampling temperature for a server-mode agent (`opencode`), sent on every `session send`. `0` = greedy/deterministic decoding (the eval rig's variance knob). Ignored by PTY agents. |
| `--parent ID` | — | The session this run forked from. Carried to the lifecycle event as `parent_session_id` and to OTel as `parent_span_id`, so a forked trace stitches across pillboxes. Observability metadata — the parent need not exist in this pillbox. |

Env composition order (later layers override earlier):

```
--env (lowest)  →  --env-file  →  --with (highest)
```

If a layer shadows an earlier variable, pillbox emits one note to
stderr:

```
pillbox: note: ANTHROPIC_API_KEY shadowed by --with
```

#### Agent sandbox defaults (claude)

The sandbox is the isolation boundary, so a `claude` run is launched
non-interactive-friendly by default: pillbox **pre-trusts** the mounted
workspace (seeds `~/.claude.json` so the trust dialog doesn't block) and passes
**`--permission-mode auto`** (so per-tool prompts don't stall a seeded or
driven session). Override the mode by passing your own after `--` (e.g.
`pillbox run -- --permission-mode plan`); it wins. Seed an interactive turn with
the agent's positional prompt: `pillbox run --agent claude -- "your prompt"`
(interactive, *not* `-p`). Full `--dangerously-skip-permissions` is refused by
claude as root (the runner runs as root); `auto` is the strongest mode that
works today.

### `pillbox secret add` flags

| Flag | Purpose |
|---|---|
| `--from-env VAR` | Read value from host env var instead of stdin. |
| `--if-not-exists` | Fail if the secret already exists in the chosen scope. |
| `--global` | Write to the global pillbox (default: resolved pillbox). |
| `--vault` | Mark as vaulted (stub-swap at injection time). |
| `--maps-to KNOWN` | Alias to a known name's vault config (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`). |
| `--host H` `--header-scheme {x-api-key\|authorization-bearer}` `--prefix P` | Vault metadata for a custom name (all three required together). |

### Sessions — detach + reattach

A `pillbox run --detach` session can be left running and reconnected to
later. This works for both local backends (Docker and libkrun). Local
`--detach` does NOT support `--vault` (the host-side proxy can't outlive
the CLI).

| Action | How |
|---|---|
| Start in the background | `pillbox run --detach [--label TEXT]` — prints the new session id, agent keeps running. |
| Detach from an interactive run | `Ctrl-A D` from the local terminal. Sandbox keeps running; pillbox returns. |
| List | `pillbox session list` — id, attached/detached, agent, started_at, label. |
| Reattach | `pillbox session attach ID` (id or unique ≥4-char prefix). |
| Detach from another shell | `pillbox session detach ID` — SIGTERMs the local pillbox that's attached. Exits 0 if already detached. |
| Tear down | `pillbox session rm ID` — kills the sandbox and removes the local record. |

The detach hotkey is `Ctrl-A D` (matches GNU screen). `Ctrl-A Ctrl-A`
sends a literal Ctrl-A through to the sandbox PTY so readline's
beginning-of-line still works inside the agent.

Sessions are NOT inherited across pillboxes — they live in the
pillbox that started them. A project pillbox's `session list` shows
only its own sessions; the global pillbox's list shows global ones.

---

## Inheritance rules

| Resource | Read | Write |
|---|---|---|
| Secrets | project + global, project wins on conflict | resolved pillbox (or `--global`) |
| Env bundles | project + global, project wins on conflict | resolved pillbox (or `--global`) |
| Agent auth | global only | global only |
| Vault state | per-pillbox | per-pillbox |
| Sessions | per-pillbox (no inheritance) | resolved pillbox |

From a global pillbox, reads see only global. From a project pillbox,
reads merge global into project (project wins on overlap).

---

## Exit codes (depend on these)

| Code | Meaning | Examples |
|---|---|---|
| 0 | Success | — |
| 1 | Runtime error, recoverable | secret not found, login expired, agent exited non-zero |
| 2 | Usage error | bad flag, unknown subcommand, mutually-exclusive flags |
| 3 | Configuration error | corrupt secret store, `.env` parse failure, v0.5 layout detected |
| 4 | Resource not ready | Docker daemon down, runner image missing |

Stable across v0.6. Pillbox scripts can rely on these.

---

## Error message format

```
pillbox: <action> failed. <reason>.
  Next: <exact command to run>
```

Example:

```
pillbox: run failed. no stored credentials for `claude`.
  Next: pillbox auth login --agent claude
```

---

## JSON output schemas (`--json`)

All `--json` outputs include a `version` field. Add fields freely in
future releases; the version bumps on restructure. Pin against
`version: 1` for now.

```jsonc
// pillbox list --json
{
  "version": 1,
  "pillboxes": [
    { "name": "global", "scope": "global", "state_dir": "/Users/x/.pillbox/global" },
    { "name": "myapp",  "scope": "project",
      "key": "-Users-x-work-myapp",
      "source_dir": "/Users/x/work/myapp",
      "state_dir":  "/Users/x/.pillbox/projects/-Users-x-work-myapp",
      "agent": "claude",
      "created_at": "2026-05-19T17:30:00Z" }
  ]
}

// pillbox info --json
{
  "version": 1,
  "pillbox": { "name": "myapp", "scope": "project", ... },
  "from_pillbox_toml": true   // false when discovery fell back to global
}

// pillbox secret list --json
// `scope` = "global" or the project's display name. Project secrets that
// shadow a global one show as project-scoped.
{
  "version": 1,
  "pillbox": "myapp",
  "secrets": [
    { "name": "ANTHROPIC_API_KEY", "scope": "global",
      "vault": { "host": "api.anthropic.com", "scheme": "x-api-key" } },
    { "name": "OPENAI_API_KEY", "scope": "myapp" }
  ]
}

// pillbox secret show NAME --json
{
  "version": 1,
  "name": "ANTHROPIC_API_KEY",
  "value": "sk-ant-***",
  "revealed": false,
  "source": "global",
  "vault": { "host": "...", "scheme": "..." }
}

// pillbox env list --json
{
  "version": 1,
  "pillbox": "myapp",
  "bundles": [
    { "name": "prod",  "scope": "myapp",  "variable_count": 7 },
    { "name": "stage", "scope": "global", "variable_count": 5 }
  ]
}

// pillbox auth list --json
{
  "version": 1,
  "agents": [
    { "id": "claude", "home": "/Users/x/.pillbox/global/auth/claude", "authenticated": true }
  ]
}

// pillbox doctor --json
{
  "version": 1,
  "checks": [
    { "name": "docker_daemon", "ok": true, "detail": "Docker 24.0.7" },
    { "name": "runner_image", "ok": true, "detail": "pillbox:latest (...)" },
    { "name": "data_dir_perms", "ok": true, "detail": "/Users/x/.pillbox mode 700" }
  ],
  "overall_ok": true
}

// pillbox snapshot list --json
{
  "version": 1,
  "pillbox": "myapp",
  "snapshots": [
    {
      "handle": "<64-char hex>",
      "short": "<first 8 chars>",
      "created_at": "2026-05-20T17:30:00Z",
      "tag": "v1",
      "message": "first cut",
      "git_anchor": "abc123...",
      "git_dirty": false,
      "parents": ["<64-char hex>", ...],   // lineage DAG edges; [] if none
      "bytes": 1024
    }
  ]
}

// pillbox snapshot show HANDLE --json  (also: pillbox push --json)
{
  "version": 1,
  "snapshot": { "handle": "...", "short": "...", "created_at": "...",
                "tag": null, "message": null,
                "git_anchor": null, "git_dirty": false, "parents": [], "bytes": 0 }
}

// pillbox bookmark list --json
{
  "version": 1,
  "pillbox": "myapp",
  "bookmarks": [
    { "name": "main", "snapshot": "<64-char hex>", "short": "<first 8>",
      "created_at": "2026-05-20T17:30:00Z",
      "updated_at": "2026-05-21T09:00:00Z" }
  ]
}

// pillbox bookmark show NAME --json
{
  "version": 1,
  "bookmark": { "name": "main", "snapshot": "<64-char hex>", "short": "<first 8>",
                "created_at": "...", "updated_at": "..." }
}

// pillbox vault status --json
{
  "version": 1,
  "ca_exists": true,
  "ca_dir": "/Users/x/.pillbox/projects/.../vault",
  "ca_cert_path": "/Users/x/.pillbox/projects/.../vault/pillbox-vault-ca.crt",
  "pillbox": "myapp"
}
```

---

## Migrating from v0.5

v0.6 is a **hard reset**. No migration shim. If `~/.pillbox/` still has
the v0.5 layout (`data/`, `secrets/`, `env/`, or `vault/` at the top
level), pillbox refuses to run and points at the recovery:

```
mv ~/.pillbox ~/.pillbox.v0.5-backup
pillbox init
pillbox auth login --agent claude
# ... re-add secrets, env bundles, etc.
```

v0.5 command shapes that break in v0.6:

| v0.5 | v0.6 |
|---|---|
| `pillbox claude run` | `pillbox run --agent claude` (or just `pillbox run`) |
| `pillbox claude login` | `pillbox auth login --agent claude` |
| `pillbox secret add NAME` | `pillbox secret add NAME` (scoped to current pillbox) |
| `pillbox config` | `pillbox info` |
| pillbox.toml `with = [...]` `mount = [...]` `env_file = [...]` `env = "..."` | dropped — CLI-only in v0.6 |

---

## Anti-patterns

- Don't commit `~/.pillbox/` to git (plaintext secrets).
- Don't reach for `--global` reflexively — most secrets belong in the
  project pillbox where they can be `rm`'d cleanly when the project
  retires.
- Don't expect v0.5 state to migrate. Back up + re-add.
- Do use `pillbox doctor --json` as the first call in a fresh
  environment.

---

## Pillbox version this guide describes

Two version namespaces, on purpose:

- **Design milestone** — `v0.6` (pillbox-as-bundle reshape + workspace
  versioning + sessions, local-only), since extended by the libkrun pivot and
  the **§0 multiplayer** layer. The doc headings track this label.
- **Published crate / `pillbox version`** — `0.2.0` (git tags `v0.1.0`,
  `v0.2.0`). This is what `pillbox version` reports; it is *not* the milestone
  label, so don't expect them to match.

If a command shape here disagrees with your binary, trust `pillbox --help`.
