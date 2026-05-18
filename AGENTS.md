# pillbox — agent guide

This file is for **coding agents** (Claude Code, Codex, opencode, etc.) using
pillbox on a user's behalf. It explains the mental model in one screen and
documents every command an agent might need to run.

If you're a human, the README is friendlier. If you're an agent, this is what
you want.

---

## Mental model — 5 things

| Concept | What it is | Where it lives |
|---|---|---|
| **agent** | A coding CLI pillbox sandboxes (`claude`, `codex`) | `~/.pillbox/data/<agent>/` |
| **secret** | A single named value bound to one env var | `~/.pillbox/secrets/<name>` |
| **env** | A named bundle of secrets, `.env`-file shaped | `~/.pillbox/env/<name>` |
| **workspace** | A host directory mounted at `/workspace/<name>` inside the sandbox | passed at run-time |
| **config** | Per-project defaults for `run` flags | `./pillbox.toml` (walks up from cwd) |

The whole CLI is **subject → verb**: `pillbox <subject> <verb> [args]`.
Pattern-match on that and you'll predict every command correctly.

---

## Quick start (the only flow you'll usually need)

```sh
pillbox claude login                              # one-time per agent (OAuth in browser)
pillbox secret add ANTHROPIC_API_KEY              # one-time per secret (paste at prompt)
pillbox env load prod path/to/.env.prod           # one-time per bundle
pillbox claude run --env prod                     # every invocation, mounts cwd at /workspace/<basename>
```

Stop reading if that's all you need. The rest is reference.

---

## Command surface

### Agents (subject = `claude` | `codex`)

| Command | What it does |
|---|---|
| `pillbox <agent> login` | OAuth flow in a one-shot Docker sandbox; persists state to `~/.pillbox/data/<agent>/` |
| `pillbox <agent> run [opts] [-- agent-args]` | Boot a sandbox with state mounted + cwd mounted at `/workspace/<name>`; attach PTY |

`run` flags:

| Flag | Default | Purpose |
|---|---|---|
| `--workspace PATH` | cwd | Host directory to mount as the workspace |
| `--name NAME` | `basename(workspace)` | Override the mount-point name (`/workspace/NAME`) |
| `--mount HOST:GUEST` | — | Extra bind mount, repeatable. Forwarded to `docker -v` |
| `--with NAME[=ENV_VAR]` | — | Inject one stored secret. `NAME` alone means `NAME=NAME`. Repeatable. |
| `--env BUNDLE` | — | Inject every variable from a stored env bundle |
| `--env-file PATH` | — | Inject every variable from a `.env` file on disk (no persistence) |
| `--strict` | off | Use a Gondolin microVM instead of Docker. v0.4 ships the flag; errors with "unavailable in this build". Real impl in v0.5. See [docs/strict.md](./docs/strict.md). |
| `--config PATH` | — | Use a specific pillbox.toml (disables discovery) |
| `--no-config` | — | Skip pillbox.toml discovery entirely |
| `--vault` | — | Route Anthropic API traffic through the pillbox stub-swap proxy. `claude` only in v0.4. See [docs/vault.md](./docs/vault.md). |

Defaults from `./pillbox.toml` (or any ancestor directory) are applied first, then CLI flags. Multi-value flags (`--with`, `--mount`, `--env-file`) append to the file's list. Single-value flags (`--name`, `--env`) override the file's value. See [docs/config.md](./docs/config.md) for the full schema.

Env composition order (later flags override earlier ones):

```
--env (lowest precedence)  →  --env-file  →  --with (highest)
```

If a later layer shadows an earlier variable, pillbox emits one line to stderr:
```
pillbox: note: ANTHROPIC_API_KEY shadowed by --with
```

### Secrets (subject = `secret`)

| Command | What it does |
|---|---|
| `pillbox secret add NAME` | Read value from stdin, store at `~/.pillbox/secrets/NAME` (0600). Overwrites silently. |
| `pillbox secret add NAME --from-env VAR` | Read value from `$VAR` in the host environment |
| `pillbox secret add NAME --if-not-exists` | Exit 1 if NAME already exists (use to gate "create-only" agent flows) |
| `pillbox secret list [--json]` | List secret names |
| `pillbox secret show NAME [--json]` | Show NAME's value, **masked by default** (`sk-ant-***`) |
| `pillbox secret show NAME --reveal` | Show plain value. Refuses if stdout is not a TTY unless `--to-stdout` is also passed. |
| `pillbox secret rm NAME` | Delete the secret |

### Environment bundles (subject = `env`)

| Command | What it does |
|---|---|
| `pillbox env load NAME PATH` | Parse `.env`-formatted file at `PATH`, store as bundle `NAME` |
| `pillbox env load NAME PATH --if-not-exists` | Exit 1 if NAME already exists |
| `pillbox env list [--json]` | List bundle names |
| `pillbox env show NAME [--json] [--reveal]` | List variables in the bundle, values masked by default |
| `pillbox env rm NAME` | Delete the bundle |

### Auth state (subject = `auth`)

| Command | What it does |
|---|---|
| `pillbox auth list [--json]` | Show which agents have stored login state |
| `pillbox auth rm AGENT` | Wipe an agent's stored state (forces re-login next run) |

### Vault (subject = `vault`)

| Command | What it does |
|---|---|
| `pillbox vault ca [--json]` | Print path to the CA cert (creates it on first call) |
| `pillbox vault status [--json]` | Report whether a CA is on disk and where |

### Operational (subject = `config` | `doctor` | `version`)

| Command | What it does |
|---|---|
| `pillbox config [--json]` | Show the resolved `pillbox.toml` for the current directory (or that none was found) |
| `pillbox doctor [--json]` | Diagnose environment: Docker running, image present, perms OK, `$HOME` resolvable |
| `pillbox version` | Print pillbox version + the image tag it targets |

---

## Exit codes (depend on these)

| Code | Meaning | Examples |
|---|---|---|
| 0 | Success | — |
| 1 | Runtime error, recoverable | secret not found, login expired, agent exited non-zero |
| 2 | Usage error | bad flag, unknown subcommand, mutually-exclusive flags |
| 3 | Configuration error | corrupt secret store, `.env` parse failure |
| 4 | Resource not ready | Docker daemon down, runner image missing |

If you're scripting pillbox, you can rely on these. They're part of the public contract.

---

## Error message format

All actionable errors follow:
```
pillbox: <action> failed. <reason>.
  Next: <exact command to run>
```

Example:
```
pillbox: claude run failed. No stored credentials for `claude`.
  Next: pillbox claude login
```

If you see an error without a `Next:` line, the failure is something pillbox can't suggest an action for (e.g., Docker permission denied — depends on the host).

---

## JSON output schemas (`--json`)

All `--json` outputs include a `version` field. Add fields freely in future releases; the version bumps on restructure. Pin against `version: 1` for now.

```jsonc
// pillbox secret list --json
{
  "version": 1,
  "secrets": [{ "name": "ANTHROPIC_API_KEY", "created_at": "2026-05-18T03:24:08Z" }]
}

// pillbox secret show NAME --json
{
  "version": 1,
  "name": "ANTHROPIC_API_KEY",
  "value": "sk-ant-***",       // masked unless --reveal
  "created_at": "2026-05-18T03:24:08Z"
}

// pillbox env list --json
{
  "version": 1,
  "bundles": [{ "name": "prod", "variable_count": 7, "created_at": "..." }]
}

// pillbox env show NAME --json
{
  "version": 1,
  "name": "prod",
  "variables": [
    { "key": "DATABASE_URL", "value": "postgres://***" },
    { "key": "REDIS_URL", "value": "redis://***" }
  ]
}

// pillbox auth list --json
{
  "version": 1,
  "agents": [{ "id": "claude", "home": "/Users/x/.pillbox/data/claude", "authenticated": true }]
}

// pillbox doctor --json
{
  "version": 1,
  "checks": [
    { "name": "docker_daemon", "ok": true, "detail": "Docker 24.0.7" },
    { "name": "runner_image", "ok": true, "detail": "pillbox:latest" },
    { "name": "data_dir_perms", "ok": true, "detail": "/Users/x/.pillbox/ mode 0700" }
  ],
  "overall_ok": true
}

// pillbox config --json
// `config.source` is null when no pillbox.toml was found between cwd and /.
{
  "version": 1,
  "config": {
    "source": "/Users/x/work/myapp/pillbox.toml",
    "name": "myapp",
    "env": "dev",
    "with": ["ANTHROPIC_API_KEY"],
    "mount": ["/Users/x/.aws:/home/lum/.aws:ro"],
    "env_file": [".env.local"]
  }
}

// pillbox vault ca --json
{
  "version": 1,
  "ca_cert_path": "/Users/x/.pillbox/vault/pillbox-vault-ca.crt"
}

// pillbox vault status --json
// ca_cert_path is null when ca_exists is false.
{
  "version": 1,
  "ca_exists": true,
  "ca_dir": "/Users/x/.pillbox/vault",
  "ca_cert_path": "/Users/x/.pillbox/vault/pillbox-vault-ca.crt"
}
```

---

## Recipes

**Spawn claude with a single API key and the cwd mounted**
```sh
pillbox secret add ANTHROPIC_API_KEY              # paste, then enter
pillbox claude run --with ANTHROPIC_API_KEY
```

**Spawn codex against a staging environment**
```sh
pillbox env load staging .env.staging
pillbox codex run --env staging
```

**Run claude with a staging env plus one secret override**
```sh
pillbox claude run --env staging --with OPENAI_KEY=OPENAI_API_KEY
# --with wins, you'll see: "pillbox: note: OPENAI_API_KEY shadowed by --with"
```

**Set up a fresh project from a script (idempotent)**
```sh
pillbox secret add ANTHROPIC_API_KEY --if-not-exists < /dev/null || true
pillbox env load prod .env.prod --if-not-exists || true
pillbox doctor
pillbox claude run --env prod
```

**Discover what's available before running**
```sh
pillbox auth list --json
pillbox secret list --json
pillbox env list --json
pillbox doctor --json
```

**Read a secret's real value to pass elsewhere**
```sh
ANTHROPIC_API_KEY=$(pillbox secret show ANTHROPIC_API_KEY --reveal --to-stdout) ./my-tool
```

---

## Anti-patterns

- ❌ Don't commit `~/.pillbox/` to git (it contains plaintext secrets)
- ❌ Don't `pillbox secret show --reveal` into application logs without `--to-stdout` — pillbox refuses, on purpose
- ❌ Don't bind-mount the user's real `~/.ssh` blindly. Use a secret + agent forwarding, or an explicit `--mount ~/.ssh:/home/lum/.ssh:ro` per session
- ❌ Don't assume `secret add` is gated on existence — it overwrites by default. Use `--if-not-exists` if that matters
- ✅ Do use `pillbox doctor --json` as the first call when an agent's spawned in a new environment
- ✅ Do prefer `--env <bundle>` over many `--with` flags when shipping a whole environment

---

## Common errors and what to do

| Error | What it means | Fix |
|---|---|---|
| `No stored credentials for 'claude'` | Agent has never been logged in | `pillbox claude login` |
| `Docker daemon isn't running` | Docker Desktop is not started | Start Docker, then retry |
| `runner image 'pillbox:latest' not found locally` | Image hasn't been built | Today: `cd ~/code/lum && bun run build:runtime-image:pillbox`. v0.4 will publish to GHCR. |
| `Secret 'FOO' not found` | Trying to inject an unknown secret | `pillbox secret add FOO` |
| `Environment bundle 'staging' not found` | Trying to use an unknown bundle | `pillbox env load staging .env.staging` |
| `Refusing to reveal secret to non-TTY stdout` | Piping `secret show --reveal` to a non-terminal | Add `--to-stdout` if you really mean it |

---

## What pillbox is NOT

- Not a secrets manager. It stores plaintext at 0600. Your disk encryption (FileVault / LUKS / BitLocker) is the at-rest defense.
- Not a vault. v0.3 mounts the real secret value into the guest. v0.4 ships the stub-and-proxy-swap tier for API-key isolation.
- Not multi-user. One secret store per OS user.
- Not a CI/CD tool. Use it locally; in CI, set env vars the conventional way.

If you need any of those, pillbox isn't the right tool yet.

---

## Pillbox version this guide describes

v0.3.x. If `pillbox version` reports something else, command shapes may differ.
