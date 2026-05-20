# pillbox — agent guide

This file is for **coding agents** (Claude Code, Codex, opencode, etc.)
using pillbox on a user's behalf. It explains the mental model in one
screen and documents every command an agent might need to run.

If you're a human, the README is friendlier. If you're an agent, this is
what you want.

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
| `pillbox doctor [--json]` | Diagnose Docker, image, perms, `$HOME`. |
| `pillbox version` | Print pillbox + runner image versions. |
| `pillbox push [--tag T] [--message M] [--json]` | Snapshot cwd into the pillbox's rustic repo. |
| `pillbox pull [--snapshot HANDLE]` | Restore cwd from a snapshot (defaults to latest). |
| `pillbox snapshot list [--json]` | List every snapshot in the pillbox's repo. |
| `pillbox snapshot show HANDLE [--json]` | Show one snapshot (HANDLE may be a unique prefix). |
| `pillbox snapshot rm HANDLE` | Forget a snapshot (data packs survive until prune). |
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

Env composition order (later layers override earlier):

```
--env (lowest)  →  --env-file  →  --with (highest)
```

If a layer shadows an earlier variable, pillbox emits one note to
stderr:

```
pillbox: note: ANTHROPIC_API_KEY shadowed by --with
```

### `pillbox secret add` flags

| Flag | Purpose |
|---|---|
| `--from-env VAR` | Read value from host env var instead of stdin. |
| `--if-not-exists` | Fail if the secret already exists in the chosen scope. |
| `--global` | Write to the global pillbox (default: resolved pillbox). |
| `--vault` | Mark as vaulted (stub-swap at injection time). |
| `--maps-to KNOWN` | Alias to a known name's vault config (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`). |
| `--host H` `--header-scheme {x-api-key\|authorization-bearer}` `--prefix P` | Vault metadata for a custom name (all three required together). |

---

## Inheritance rules

| Resource | Read | Write |
|---|---|---|
| Secrets | project + global, project wins on conflict | resolved pillbox (or `--global`) |
| Env bundles | project + global, project wins on conflict | resolved pillbox (or `--global`) |
| Agent auth | global only | global only |
| Vault state | per-pillbox | per-pillbox |

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

v0.6 (PR 2 — pillbox-as-bundle reshape). If `pillbox version` reports
something else, command shapes may differ.
