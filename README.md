# pillbox

**Sandboxed coding agents that travel with their state.** A pillbox
bundles a workspace, code, credentials, and vault config into one
self-contained unit. Make one for your machine, one per project, and
launch `claude` or `codex` against it — locally in Docker, on a
registered VPS over SSH, or in an E2B managed sandbox. Same bundle,
three runtimes.

```sh
pillbox init                          # one-time: create the global pillbox
pillbox auth login --agent claude     # one-time per agent (OAuth in a sandbox)

cd ~/work/my-project
pillbox new --name my-project         # writes pillbox.toml + per-project state
pillbox run                           # sandbox + claude + cwd mounted
```

That's the happy path. The rest of this file is reference.

## Why "pillbox-as-bundle"

Coding agents need credentials, state, and a workspace to act on.
Mixing those into one bundle gives you:

- **One mental model.** A pillbox is the thing you create, list, run,
  remove. `claude` / `codex` / `secret` / `env` / `auth` / `vault` are
  not top-level subjects — they're per-pillbox concerns.
- **Project isolation.** Per-project secrets, per-project env bundles,
  per-project vault state, per-project workspace history. One
  project's leases never collide with another's.
- **Shared agent auth.** A single `pillbox auth login --agent claude`
  lives in the global pillbox and is reused across every project.
- **Local or remote, same surface.** Add a remote with
  `pillbox remote add NAME ssh://user@host` (or `e2b://TEMPLATE_ID`),
  then `pillbox run --remote NAME`. The pillbox is the unit that
  travels — workspace pulled from an S3-shaped store, vault material
  delivered once over the encrypted channel.

## Where state lives

```
~/.pillbox/                                # 0700
├── global/                                # global pillbox
│   ├── secrets/                           # cross-project secrets (project shadows)
│   ├── env/                               # cross-project env bundles
│   ├── auth/{claude,codex}/               # agent OAuth state (always global today)
│   ├── vault/                             # CA + key for vault sessions
│   ├── remotes/                           # registered VPS / E2B remotes
│   └── sessions/                          # detached session records (local Docker, E2B, SSH)
├── projects/
│   └── -Users-vuln-work-foo/              # `/Users/vuln/work/foo` with `/` → `-`
│       ├── meta.json                      # { name, created_at, agent_default, workspace }
│       ├── secrets/                       # overrides global on key conflict
│       ├── env/
│       ├── auth/                          # reserved (per-project auth → v0.7)
│       ├── vault/
│       ├── remotes/                       # per-pillbox remote overrides
│       ├── sessions/                      # sessions started from this pillbox
│       ├── repo-password                  # 0600 — rustic repo encryption password
│       └── repo/                          # local rustic repository (local backend only)
└── cache/                                 # versioned helper scripts (e.g. e2b-helper-vX.mjs)
```

The state-dir key is the absolute path of the directory holding
`pillbox.toml`, with `/` replaced by `-`. Human-readable, greppable,
unique per host.

## pillbox.toml

```toml
# Required
name = "my-project"

# Optional — default agent for `pillbox run`
agent = "claude"          # or "codex"

# Workspace backend (default: local)
[workspace]
backend = "local"         # or "s3"
# s3-only:
# endpoint = "https://<account>.r2.cloudflarestorage.com"
# region = "auto"
# bucket = "my-bucket"
# prefix = "pillbox/"
# access_key_env = "R2_ACCESS_KEY"   # env-var NAME, not the secret value
# secret_key_env = "R2_SECRET_KEY"
```

Discovery walks up from cwd looking for `pillbox.toml` (like
`.gitignore` or `Cargo.toml`). First match wins. Pass `--pillbox NAME`
to bypass discovery and operate on a specific named pillbox.

## Command surface

### Lifecycle

| Command | What it does |
|---|---|
| `pillbox init` | Create the global pillbox at `~/.pillbox/global/`. Idempotent. |
| `pillbox new [--name N] [--agent A] [workspace flags]` | Create a project pillbox in cwd. |
| `pillbox list [--json]` | Every pillbox on disk. |
| `pillbox rm NAME` | Delete a project pillbox by name or key. Refuses `global`. |
| `pillbox info [--json]` | Show the current pillbox. |

### Run + remotes + sessions

| Command | What it does |
|---|---|
| `pillbox run [opts] [-- args]` | Launch the agent against the current pillbox (local Docker by default). |
| `pillbox run --remote NAME [--from-bookmark B]` | Launch on a registered remote VPS (`ssh://`) or E2B sandbox (`e2b://`). |
| `pillbox run --detach [--remote NAME] [--label TEXT]` | Start the session in the background; print the reattach id. Works locally and on `ssh://` / `e2b://` remotes. |
| `pillbox remote add NAME URL [--agent A]` | Register a remote — `ssh://user@host[:port]` or `e2b://TEMPLATE_ID`. |
| `pillbox remote list/info/rm` | Manage the remote registry. |
| `pillbox session list [--json]` | Sessions started from this pillbox. |
| `pillbox session info ID [--json]` | One session (accepts unique id prefix ≥ 4 chars). |
| `pillbox session attach ID` | Reattach to a detached session. Detach again with **Ctrl-A D**. |
| `pillbox session detach ID` | Signal a currently-attached pillbox to detach (no-op if already detached). |
| `pillbox session rm ID` | Tear down the backend (kill sandbox) and remove the record. |

### Secrets / env / auth / vault

| Command | What it does |
|---|---|
| `pillbox secret add/list/show/rm [--global]` | Manage secrets (project default; `--global` writes to global). |
| `pillbox env load/list/show/rm [--global]` | Manage env bundles (same scoping). |
| `pillbox auth login/list/rm --agent A` | Manage agent OAuth state (always global). |
| `pillbox vault ca/status` | Inspect the per-pillbox vault CA. |
| `pillbox sidecar [--bind] [--json]` | Run the credential vault as a standalone process. |

### Workspace

| Command | What it does |
|---|---|
| `pillbox push [--tag T] [--message M] [--json]` | Snapshot cwd into the pillbox's rustic repository. |
| `pillbox pull [--snapshot HANDLE \| --bookmark NAME]` | Restore cwd from a snapshot or bookmark (defaults to latest). |
| `pillbox snapshot list/show/rm` | Manage snapshots in the pillbox's repo. |
| `pillbox bookmark list/show/set/rm` | Manage named bookmarks that point at snapshots. |
| `pillbox workspace rekey` | Rotate the rustic repo encryption password. |

### Other

| Command | What it does |
|---|---|
| `pillbox doctor [--json]` | Diagnose Docker, image, perms, `$HOME`. |
| `pillbox version` | Print pillbox + runner-image versions. |
| `pillbox completions SHELL` | Emit shell completions (`bash`, `zsh`, `fish`, …). |

`--pillbox NAME` is global — works on every per-pillbox command to
override cwd-based discovery.

## Inheritance rules

| What | Read | Write default | `--global` |
|---|---|---|---|
| Secrets | project + global (project wins) | resolved pillbox | force global |
| Env bundles | project + global (project wins) | resolved pillbox | force global |
| Remotes | project + global (project wins) | resolved pillbox | force global |
| Auth | global | global | (implicit; accepted for fwd-compat) |
| Vault | per-pillbox | per-pillbox | n/a |
| Sessions | per-pillbox (no inheritance) | resolved pillbox | n/a |

A project pillbox always sees the global pillbox as a fallback for
secrets / env / remotes. Sessions are runtime state and stay tied to
the pillbox that started them.

## Remote backends

```sh
# SSH to a VPS you already installed pillbox on:
pillbox remote add prod-vps ssh://deploy@vps.example.com
pillbox run --remote prod-vps

# E2B managed sandbox (requires `node` + `npm i -g e2b` + $E2B_API_KEY):
pillbox remote add prod-cloud e2b://my-pillbox-template
pillbox run --remote prod-cloud

# Backgrounded session — reattach later from any shell:
pillbox run --remote prod-cloud --detach --label "nightly-build"
# pillbox: ✓ session `abc123def456` started in background.
#          pillbox session attach abc123def456  # reattach

pillbox session attach abc123def456     # streams the PTY back; Ctrl-A D detaches
pillbox session detach abc123def456     # from another shell
pillbox session rm abc123def456         # kill sandbox + remove record
```

**Workspace handoff:** remote runs require an S3-shaped workspace
backend in v0.6 — the local pillbox and the remote share the same
bucket / endpoint. The launch path snapshots cwd as the remote base
unless `--from-bookmark` is passed; the remote hydrates that snapshot
before the agent starts and pushes a result snapshot after it exits.
Local-rustic transport over the wire is a planned PR 4.1.

**Vault handoff:** real secret values cross the network once, over
the encrypted channel (SSH stdin or the E2B Files API), into the
remote pillbox's vault session memory. The blob is never written to
disk on the SSH path; E2B stages it through 0600 temp files locally
and in sandbox `/tmp`, then unlinks after the in-sandbox pillbox reads it.

**SSH vs E2B parity:** detached sessions (`--detach`,
`session attach`/`detach`/`rm`) work uniformly across local Docker,
E2B, and SSH remotes — each carries the attach-transport frames over
its own byte pipe (docker exec, E2B's `pty.connect`, ssh stdio).

## Sessions and the detach hotkey

When you attach to a session (initial run OR `pillbox session
attach`), the local terminal proxies the session's PTY (local Docker,
e2b, or ssh). To detach **without killing the session**:

- **Ctrl-A then D** — works from the attached terminal.
- `pillbox session detach <id>` — works from any shell.

Either way the session record stays in the registry; the backend
keeps running until `pillbox session rm <id>`. Inside the sandbox,
Ctrl-A Ctrl-A delivers a literal Ctrl-A to the PTY (so readline /
shell beginning-of-line still works).

## Hard reset from v0.5

v0.6 is a deliberate identity reset. There is no migration shim. If
`~/.pillbox/` contains the v0.5 layout (`data/`, `secrets/`, `env/`,
or `vault/` at the top level), v0.6 refuses to run and prints a
pointer like:

```
pillbox: pillbox init failed. detected v0.5 pillbox state (~/.pillbox/data/, …). v0.6 is a hard reset — no migration shim.
  Next: mv ~/.pillbox ~/.pillbox.v0.5-backup && pillbox init
```

Back up, init, re-login. Auth state, secrets, and env bundles do not
carry over.

## Threat model (one screen — see [docs/security.md](./docs/security.md) for the full version)

Pillbox **defends** against:

- An agent reading host environment variables, the user's real
  `~/.claude` / `~/.codex` / `~/.gh`. The sandbox only mounts the
  resolved pillbox's auth dir.
- The login flow contaminating future runs. Login containers are
  one-shot.
- Host tools accidentally consuming pillbox state — everything is
  namespaced under `~/.pillbox/`.
- Workspace data leaking through cloud backends. Rustic encrypts
  on the client; a stolen bucket alone can't be decrypted (password
  is local-only at `<state_dir>/repo-password`).
- Real API keys reaching the agent process. `--vault` routes
  traffic through a stub-swap MITM so leaked API keys are useless.

Pillbox **does not** defend against:

- A prompt-injected agent exfiltrating credentials it was given on
  purpose. `--vault` makes API keys useless but subscription tokens
  are still mountable.
- Stolen unencrypted disk / backups. Files are plaintext at 0600.
  Disk encryption (FileVault / LUKS / BitLocker) is the at-rest
  defense.
- Container escape or kernel attacks on the local Docker backend.
  Remote backends (E2B microVMs, VPSes) move execution to a
  hardware-isolated host when this is the wrong trust boundary.

Same posture as `gh`, `aws`, `docker`, `kubectl`. Pillbox is a
sandbox runner, not a secrets manager.

## Status

Pre-alpha. v0.6 is the major reshape (pillbox-as-bundle identity +
remote backends + sessions). Roadmap:

- **v0.1–v0.5** ✅ Claude / Codex sandboxing, secrets + env bundles,
  pillbox.toml v1, credential vault (Anthropic + Codex + API keys), CI.
- **v0.6 PR 1** ✅ `SandboxBackend` trait + sidecar mode.
- **v0.6 PR 2** ✅ Pillbox-as-bundle CLI redesign.
- **v0.6 PR 3** ✅ Workspace backends (`rustic_core` — local + S3/R2).
- **v0.6 PR 4** ✅ RemoteSsh backend.
- **v0.6 PR 5** ✅ RemoteE2b backend.
- **v0.6 PR 6** ✅ Sessions (list/attach/detach) across local Docker,
  E2B, and SSH.
- **v0.6 PR 7** ✅ Polish + docs/README rewrite (this one).
- **v0.7+** local-rustic workspace handoff over the wire (PR 4.1),
  `PerPillboxRegistry<T>` lift, optional cloud sidecar for the vault.

## Build

```sh
# Pull the canonical runner image (or build your own — see docs/runner-image.md)
docker pull ghcr.io/vu1n/pillbox-runner:latest

# Build + install the CLI
cd ~/code/pillbox && cargo install --path .

# First use
pillbox doctor                          # green?
pillbox init
pillbox auth login --agent claude
cd ~/work/my-project && pillbox new && pillbox run
```

To use a custom image (extra tools, newer harnesses):

```sh
PILLBOX_RUNNER_IMAGE=my-team/pillbox-runner:custom pillbox run
# or pin per-pillbox in pillbox.toml's [runner] table
```

See [docs/runner-image.md](docs/runner-image.md) for the image
contract, build recipe, and how Renovate keeps the harnesses
current.

## Documentation

- [AGENTS.md](./AGENTS.md) — agent-facing command reference (machine-readable)
- [CHANGELOG.md](./CHANGELOG.md) — what shipped, by milestone
- [docs/](./docs/) — topic deep dives
  - [config.md](./docs/config.md) — `pillbox.toml` descriptor + discovery
  - [secrets.md](./docs/secrets.md) — pillbox-scoped secrets + env bundles
  - [vault.md](./docs/vault.md) — per-pillbox credential vault
  - [shared-mcp.md](./docs/shared-mcp.md) — `--mcp NAME=URL` shared-MCP attachments
  - [remotes.md](./docs/remotes.md) — remote backends + sessions
  - [runner-image.md](./docs/runner-image.md) — the runner image, overrides, custom builds
  - [recipes.md](./docs/recipes.md) — copy-paste flows
  - [security.md](./docs/security.md) — threat model
- [SECURITY.md](./SECURITY.md) — vulnerability reporting

## License

MIT OR Apache-2.0
