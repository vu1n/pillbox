# pillbox — agent guide

This file is for **coding agents** (Claude Code, Codex, opencode, etc.)
using pillbox on a user's behalf. It explains the mental model in one
screen and documents every command an agent might need to run.

If you're a human, the README is friendlier. If you're an agent, this is
what you want.

> **⚠️ Direction note (2026-06-01).** The **remote** commands below
> (`remote add`, `--remote`, `ssh://`/`e2b://`/`docker://`) still work but are
> **deprecated** — "remote" is becoming Cloudflare-managed or pillbox-running-
> locally-on-the-box, and the local sandbox runtime is pivoting **Docker →
> libkrun microVM** (`docs/libkrun-sandbox.md`). Use local `pillbox run`; don't
> build new automation on `--remote`. Everything else (run, secrets, env, auth,
> vault, sessions, snapshots) is current.

---

## Mental model — one concept

**A pillbox is a self-contained bundle of (workspace + code + vault +
config).** Users create pillboxes, then run agents against them.

| | What it is | Where it lives |
|---|---|---|
| **global pillbox** | One per OS user. Shared agent auth + fallback secrets/env. | `~/.pillbox/global/` |
| **project pillbox** | One per directory with `pillbox.toml`. Overrides global. | `~/.pillbox/projects/<dash-encoded-cwd>/` |
| **pillbox.toml** | Marks a directory as a project pillbox. Required field: `name`. | `./pillbox.toml` (walks up from cwd) |

Top-level commands act on **pillbox lifecycle** (init/new/list/rm/info).
Per-pillbox commands act on the **current** pillbox (run/secret/env/auth/
vault/...). The current pillbox is resolved by walking up from cwd
looking for `pillbox.toml`. No descriptor found → global. The `--pillbox
NAME` flag overrides discovery to point at a specific named pillbox.

---

## Quick start

```sh
# Bootstrap (one time)
pillbox init                                # creates ~/.pillbox/global/
pillbox auth login --agent claude           # OAuth in a sandbox

# Create a project pillbox
cd ~/work/myapp
pillbox new --name myapp                    # writes pillbox.toml + state

# Use it
pillbox secret add ANTHROPIC_API_KEY        # paste, then Ctrl-D
pillbox run                                 # mounts cwd at /workspace/myapp
```

Stop reading if that's all you need.

---

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
| `pillbox remote add NAME URL [--agent A] [--global]` | Register a remote for `pillbox run --remote NAME`. URL is `ssh://user@host[:port]` (VPS) or `e2b://TEMPLATE_ID` (E2B managed sandbox). The remote-side `pillbox` must already be installed on the VPS / baked into the E2B template image (we don't deploy binaries). |
| `pillbox remote list [--json]` | List remotes visible from the current pillbox (project + global, deduplicated). |
| `pillbox remote info NAME [--json]` | Show one remote (with inheritance). |
| `pillbox remote rm NAME [--global]` | Remove a registered remote. |
| `pillbox session list [--json]` | List sessions started from this pillbox (oldest first). |
| `pillbox session info ID [--json]` | Show one session (accepts unique id prefix ≥ 4 chars). |
| `pillbox session attach ID` | Reattach to a detached session. Detach again with Ctrl-A + D or `pillbox session detach ID` from another shell. Works for local Docker, e2b, and ssh sessions. |
| `pillbox session detach ID` | Signal a currently-attached pillbox to detach (SIGTERM, no-op if already detached). |
| `pillbox session send ID TEXT` | Drive a running (detached) session: push TEXT to the agent's PTY as if typed — the programmatic SendInput half (pair with `session subscribe` to read the response). Bytes sent as-is; add a trailing newline/`\r` to submit a prompt to a TUI agent. Local Docker sessions today. |
| `pillbox session subscribe ID [--from SEQ] [--bind ADDR]` | Stream a session's durable event log to WebSocket subscribers as JSON (one Event per text frame, in seq order from `--from`). For a **live** (detached) session it also tails the transcript→log while serving, so a driven detached session is readable; for a foreground/historical session it serves the existing log. Binds localhost (`--bind`, default `127.0.0.1:0` — printed) until Ctrl-C. The §0 local read surface a chat bridge / orchestrator / browser connects to without a shell. If `$PILLBOX_EVENTS_WEBHOOK` is set, also POSTs attention signals to it (read-side). |
| `pillbox session watch ID [--from SEQ]` | Render a session's event stream to **this terminal** — messages by role, tools (⚙/✓/✗), thinking, the ⏳ attention signal — the human-facing reader (`docker logs` model; `subscribe` is the machine/WS sibling). Tails a live session as it works. Ctrl-C to stop. Accepts an id prefix. |
| `pillbox session rm ID` | Tear down the backend (kill sandbox) and remove the session record. |
| `pillbox session done ID --status ok\|failed [--reason TEXT] [--exit-code N] [--trace-path PATH] [--result-snapshot HANDLE]` | Emit `session.completed` / `session.failed` to every configured sink. Invoked automatically by the in-sandbox wrapper after the agent exits (also passes `--result-snapshot` from the remote workspace push); can also be called manually. Does NOT tear down the sandbox — use `session rm` for that. |
| `pillbox session pull ID [--to DIR]` | Rehydrate a session's result workspace into a directory. Reads `result_snapshot` from the session record; errors clearly if the agent hasn't finished. Default `DIR` is `./session-<id>`. |
| `pillbox session score ID --cmd "VERIFIER" [--snapshot HANDLE \| --workspace DIR]` | **Externally grade** a session's result — the verifiable, non-self-reported reward channel (vs `session done --status`, which is the agent's self-report, Goodhart-banned). Runs `VERIFIER` via `sh -c` with cwd = the rehydrated `result_snapshot` (or `--snapshot`/`--workspace`), captures its **exit code + output**, and appends a `scored` §0 event (exit 0 → passed/score 1.0, else 0.0; combined output → `feedback`, tail-capped at 32K). The grader runs on the host. Read back via the session's §0 log (`session subscribe`/`watch` or the log file); the optimization loops consume these. |
| `pillbox session prune [--dry-run]` | Tear down every session whose `expires_at` is in the past (calls `session rm` per record). Sessions without `--ttl` are left alone. Intended for cron/orchestrator schedules; pillbox doesn't auto-prune. |
| `pillbox session events [--follow] [--json]` | Tail the local events stream (`<pillbox>/events.jsonl`). |
| `pillbox session transcript FILE --session-id ID [--agent claude\|codex] [--follow]` | Drain an agent-native transcript (Claude Code `~/.claude/projects/<encoded>/<uuid>.jsonl` or Codex `~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl`) and emit one OTLP child span per rendered event, parented under the session span derived from `ID`. Harness auto-detected from path; `--agent` overrides. `--follow` drains then blocks waiting on FS notifications and emits spans for each appended line (Ctrl-C to stop) — the "watch your agent think" mode. Requires `OTEL_EXPORTER_OTLP_ENDPOINT` to actually ship; parser runs regardless and reports the event count. See [docs/observability.md](./docs/observability.md). |
| `pillbox doctor [--json]` | Diagnose Docker, image, perms, `$HOME`. |
| `pillbox version` | Print pillbox + runner image versions. |
| `pillbox push [--tag T] [--message M] [--json]` | Snapshot cwd into the pillbox's rustic repo. |
| `pillbox pull [--snapshot HANDLE \| --bookmark NAME]` | Restore cwd from a snapshot (defaults to latest) or bookmark. |
| `pillbox snapshot list [--json]` | List every snapshot in the pillbox's repo. |
| `pillbox snapshot show HANDLE [--json]` | Show one snapshot (HANDLE may be a unique prefix). |
| `pillbox snapshot rm HANDLE` | Forget a snapshot (data packs survive until prune). |
| `pillbox bookmark list [--json]` | List named snapshot bookmarks. |
| `pillbox bookmark show NAME [--json]` | Show one bookmark. |
| `pillbox bookmark set NAME [HANDLE\|latest]` | Point a bookmark at a snapshot (defaults to latest). |
| `pillbox bookmark rm NAME` | Remove a bookmark; the underlying snapshot is untouched. |
| `pillbox workspace rekey` | Rotate the rustic repo password. **Caveat:** rustic_core 0.11 has no public API to delete the prior key; both passwords keep working until upstream lands deletion. Treat the old password as compromised. |

### `pillbox run` flags

| Flag | Default | Purpose |
|---|---|---|
| `--agent A` | `pillbox.toml` `agent` field, then `claude` | Agent to launch (`claude` \| `codex`). |
| `--workspace PATH` | cwd | Host directory to mount. |
| `--name NAME` | `pillbox.toml` `name`, else basename(workspace) | Mount-point name (`/workspace/NAME`). |
| `--mount HOST:GUEST` | — | Extra bind mount. Repeatable. |
| `--with NAME[=ENV_VAR]` | — | Inject one stored secret. |
| `--env BUNDLE` | — | Inject every variable from a stored env bundle. |
| `--env-file PATH` | — | Inject every variable from a `.env` on disk. |
| `--vault` | — | Route agent traffic through the stub-swap proxy. |
| `--remote NAME` | — | Run on a registered remote (`ssh://` or `e2b://`) instead of locally. Requires an S3-shaped workspace backend. The launch path snapshots the current workspace as the remote base unless `--from-bookmark` is passed. For `e2b://` remotes: `node` + `npm i -g e2b` must be installed locally and `E2B_API_KEY` must be available in the environment. |
| `--from-bookmark NAME` | — | Start from a named snapshot bookmark. Local runs first restore that bookmark into the workspace; remote runs hydrate from it instead of auto-snapshotting cwd. |
| `--detach` | — | Start the session and immediately return — the agent keeps running in the background; reattach with `pillbox session attach <id>`. Works for local Docker, e2b, and ssh remotes. Local `--detach` does NOT support `--vault` (the host-side proxy can't outlive the CLI). |
| `--events-webhook URL` | — | POST every lifecycle event to URL as JSON. Forwarded to the in-sandbox wrapper so terminal events (`session.completed`/`failed`) reach back to the orchestrator. Equivalent to `$PILLBOX_EVENTS_WEBHOOK`. See [docs/observability.md](./docs/observability.md) for the full sink reference (JSONL / webhook / OTLP via `$OTEL_EXPORTER_OTLP_ENDPOINT`). |
| `--ttl DURATION` | — | Per-session retention TTL — `30m` / `24h` / `7d` (`s`/`m`/`h`/`d` units only, max 365d). Writes `expires_at` to the record. `pillbox session prune` drops expired sessions. Requires `--detach`. |
| `--label TEXT` | — | Human label for a detached session, surfaced in `pillbox session list`. Only meaningful with `--detach`. |

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
later. This works for local Docker runs, `e2b://` remotes, and `ssh://`
remotes. Local `--detach` does NOT support `--vault` (the host-side
proxy can't outlive the CLI); remote runs (e2b + ssh) require an
S3-shaped workspace backend.

| Action | How |
|---|---|
| Start in the background | `pillbox run --detach [--remote NAME] [--label TEXT]` — prints the new session id, agent keeps running. |
| Detach from an interactive run | `Ctrl-A D` from the local terminal. Sandbox keeps running; pillbox returns. |
| List | `pillbox session list` — id, attached/detached, agent, remote, started_at, label. |
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
| Remotes | project + global, project wins on conflict | resolved pillbox (or `--global`) |
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
      "bytes": 1024
    }
  ]
}

// pillbox snapshot show HANDLE --json  (also: pillbox push --json)
{
  "version": 1,
  "snapshot": { "handle": "...", "short": "...", "created_at": "...",
                "tag": null, "message": null,
                "git_anchor": null, "git_dirty": false, "bytes": 0 }
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

v0.6 (through PR 6 — pillbox-as-bundle reshape + workspace versioning +
remote backends + sessions). If `pillbox version` reports something
else, command shapes may differ.
