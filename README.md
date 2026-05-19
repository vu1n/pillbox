# pillbox

**Sandboxed coding agents, bundled.** A pillbox is a self-contained unit
of (workspace + code + vault + config) that an agent runs against.
Create one for your machine, create one per project, run `claude` or
`codex` inside it.

```sh
pillbox init                          # one-time: create the global pillbox
pillbox auth login --agent claude     # one-time per agent (OAuth in sandbox)

cd ~/work/my-project
pillbox new --name my-project         # writes pillbox.toml + per-project state
pillbox run                           # sandbox + claude + cwd mounted in
```

That's the v0.6 model in one screenshot. The rest of this file is reference.

## Why "pillbox-as-bundle"

Coding agents need credentials, state, and a workspace to act on. Mixing
those concerns into one bundle gives you:

- **One mental model.** A pillbox is the thing you create, list, run,
  remove. No more `claude` / `codex` / `secret` / `env` / `auth` / `vault`
  as top-level subjects.
- **Project isolation.** Per-project secrets, per-project env bundles,
  per-project vault state. One project's leases never collide with
  another's.
- **Shared agent auth.** A single `pillbox auth login --agent claude`
  lives in the global pillbox and is reused across every project.
- **A path to remote.** v0.6 PR 4+ adds remote-sandbox backends; the
  pillbox is the unit that ships across the wire (workspace + vault
  state + config).

## Where state lives

```
~/.pillbox/                        # 0700
├── global/                        # global pillbox
│   ├── secrets/                   # cross-project secrets, project shadows
│   ├── env/                       # cross-project env bundles
│   ├── auth/{claude,codex}/       # agent OAuth state (always global today)
│   └── vault/                     # CA + key
└── projects/
    └── -Users-vuln-work-foo/      # `/Users/vuln/work/foo` with `/` → `-`
        ├── meta.json              # { name, created_at, agent_default }
        ├── secrets/               # overrides global on key conflict
        ├── env/
        ├── auth/                  # reserved (per-project auth → v0.7)
        └── vault/
```

The state-dir key is the absolute path of the directory holding
`pillbox.toml`, with `/` replaced by `-`. Human-readable, greppable,
unique per host.

## pillbox.toml

```toml
# required
name = "my-project"

# optional — default agent for `pillbox run`
agent = "claude"          # or "codex"

# reserved for PR 3 (workspace backends)
[workspace]
```

Discovery walks up from cwd looking for `pillbox.toml` (like `.gitignore`
or `Cargo.toml`). First match wins. Pass `--pillbox NAME` to bypass
discovery and operate on a specific named pillbox.

## Command surface (v0.6)

### Lifecycle

| Command | What it does |
|---|---|
| `pillbox init` | Create the global pillbox at `~/.pillbox/global/`. Idempotent. |
| `pillbox new [--name N] [--agent A]` | Create a project pillbox in cwd. |
| `pillbox list [--json]` | Every pillbox on disk. |
| `pillbox rm NAME` | Delete a project pillbox by name or key. Refuses to remove `global`. |
| `pillbox info [--json]` | Show the current pillbox (resolved from cwd or `--pillbox`). |

### Per-pillbox

| Command | What it does |
|---|---|
| `pillbox run [--agent A] [opts] [-- args]` | Launch the agent against the current pillbox. |
| `pillbox secret add/list/show/rm [--global]` | Manage secrets (project default; `--global` writes to global). |
| `pillbox env load/list/show/rm [--global]` | Manage env bundles (same scoping). |
| `pillbox auth login/list/rm --agent A` | Manage agent OAuth state (always global in v0.6). |
| `pillbox vault ca/status` | Inspect the per-pillbox vault CA. |
| `pillbox sidecar [--bind] [--json]` | Run the credential vault as a standalone process. |
| `pillbox doctor [--json]` | Diagnose the environment. |
| `pillbox version` | Print pillbox + runner-image versions. |

`--pillbox NAME` is global — usable on every per-pillbox command to
override cwd-based discovery.

## Inheritance rules

| What | Read | Write default | `--global` |
|---|---|---|---|
| Secrets | project + global (project wins) | resolved pillbox | force global |
| Env bundles | project + global (project wins) | resolved pillbox | force global |
| Auth | global | global | (implicit, accepted for fwd-compat) |
| Vault | per-pillbox | per-pillbox | n/a |

A project pillbox always sees the global pillbox as a fallback for
secrets and env bundles. The global pillbox sees only itself.

## Hard reset from v0.5

v0.6 is a deliberate identity reset. There is no migration shim. If
`~/.pillbox/` contains the v0.5 layout (`data/`, `secrets/`, `env/`, or
`vault/` at the top level), v0.6 refuses to run and prints:

```
pillbox: pillbox init failed. detected v0.5 pillbox state (~/.pillbox/data/, ...).
v0.6 is a hard reset — no migration shim.
  Next: mv ~/.pillbox ~/.pillbox.v0.5-backup && pillbox init  # then re-add secrets / login
```

Back up, init, re-login. Auth state, secrets, and env bundles do not
carry over.

## Threat model

Pillbox **does** defend against:

- An agent reading host environment variables or the user's real
  `~/.claude` / `~/.codex` / `~/.gh`. The sandbox only sees the resolved
  pillbox's auth dir.
- The login flow contaminating future runs. Login containers are
  one-shot.
- Other host tools accidentally consuming pillbox state — everything is
  namespaced under `~/.pillbox/`.

Pillbox **does not** defend against:

- A prompt-injected agent exfiltrating credentials it was given on
  purpose. The `--vault` proxy makes leaked API keys + OAuth tokens
  useless to an attacker; subscription tokens are still a mount.
- Stolen unencrypted disk / backups. Files are plaintext at 0600 — disk
  encryption (FileVault / LUKS / BitLocker) is the at-rest defense.
- Container escape or kernel attacks. v0.6 PR 4+ adds remote-sandbox
  backends for workloads that need stronger isolation than local Docker.

This is the same posture as `gh`, `aws`, `docker`, `kubectl`. Pillbox is
a sandbox runner, not a secrets manager.

## Status

Pre-alpha. v0.6 PR 2 is the pillbox-as-bundle CLI redesign — a major
reshape, breaking from v0.5. Roadmap:

- **v0.1–v0.5** ✅ Claude / Codex sandboxing, secrets + env bundles,
  pillbox.toml v1, credential vault (Anthropic + Codex + API keys), CI.
- **v0.6 PR 1** ✅ `SandboxBackend` trait + sidecar mode.
- **v0.6 PR 2** ✅ **Pillbox-as-bundle CLI** — this one.
- **v0.6 PR 3** Workspace backends: S3/R2 + Git + Tarball + push/pull/snapshot.
- **v0.6 PR 4** RemoteSsh backend.
- **v0.6 PR 5** RemoteE2b backend.
- **v0.6 PR 6** Sessions (list/attach/detach).
- **v0.6 PR 7** Polish + README rewrite for the remote story.

## Build

```sh
# Build the runner image (until pillbox publishes its own to GHCR)
cd ~/code/lum && bun run build:runtime-image:pillbox

# Build + install the CLI
cd ~/code/pillbox && cargo install --path .

# Use it
pillbox init
pillbox auth login --agent claude
cd ~/work/my-project && pillbox new && pillbox run
```

## Documentation

- [AGENTS.md](./AGENTS.md) — agent-facing command reference
- [docs/](./docs/) — topic deep dives
  - [secrets.md](./docs/secrets.md) — pillbox-scoped secrets + env bundles
  - [config.md](./docs/config.md) — pillbox.toml descriptor
  - [vault.md](./docs/vault.md) — per-pillbox credential vault
  - [security.md](./docs/security.md) — threat model

## License

MIT OR Apache-2.0
