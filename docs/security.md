# Security model

Pillbox is a **sandbox runner and session substrate**, not a secrets manager.
Local secret files are plaintext at 0600 under the user's home, like `gh`, `aws`,
and `kubectl`; host disk encryption is the at-rest defense.

There are two security boundaries:

- **Local:** a libkrun microVM (HVF today) isolates the agent. The guest's egress
  terminates in a host-owned userspace network stack; the host broker can replace
  credential stubs only for their intended destinations and default-deny every
  unmatched host. Workspace and credential homes are per-run CoW clones with a
  pillbox-controlled secret exclusion policy.
- **Managed, experimental:** a Cloudflare Container isolates the agent; a
  bounded Worker execution service uses D1 for claims, R2 for one immutable
  terminal artifact, and the caller's local `SessionLog` for §0. Cloudflare's
  vendor-owned Sandbox Durable Object owns container lifecycle only. Huddles
  owns collaboration and driver policy.

The local Docker **agent backend** is deprecated. Docker is still in the build
and auth trust path today: it materializes the OCI rootfs that libkrun boots and
runs the current OAuth login flow. A compromised Docker daemon can therefore
tamper with those inputs even though it does not host the normal agent run.

## What pillbox defends against

| Threat | How pillbox mitigates |
|---|---|
| Agent reads `~/.claude` or `~/.codex` on the host | Sandbox only mounts the resolved auth pillbox's `auth/<agent>/` dir; the agent never sees the host's real config directories. |
| Agent reads host environment variables | Guest env is built from explicit pillbox inputs only. The host's `$ANTHROPIC_API_KEY` etc. do not leak in. |
| Login flow contaminated by host state | The login container is one-shot and fresh — no prior state mounted. |
| Other host tools accidentally consuming pillbox state | Everything is namespaced under `~/.pillbox/` with restrictive perms. |
| `pillbox secret show --reveal` accidentally piped to logs | Refused unless `--to-stdout` is also passed. |
| Agent crosses the normal local sandbox boundary | The local agent runs in a hardware-isolated libkrun microVM, not a shared-kernel container. Guest egress terminates at the host-owned broker. |
| Managed caller invokes another run | Public Worker routes require an expiring capability bound to one operation and exact session/invocation id; Huddles uses the trusted service binding and owns participant authorization. |
| Managed workspace credential reaches outside its project | When scoping is enabled, the host mints a fresh prefix-scoped R2 credential per transfer; mint/shape failures abort rather than falling back silently. |

## What pillbox does NOT defend against

| Threat | Why pillbox can't help |
|---|---|
| Prompt-injected agent abusing an authorized capability | Isolation does not distinguish a requested action from a prompt-injected one. Stub credentials prevent raw-key theft, but an agent may still make allowed requests through the broker. Use default-deny egress and least-privilege accounts. |
| Stolen unencrypted disk / backup | Files are plaintext at 0600. If FileVault / LUKS / BitLocker isn't on, an attacker with the disk has the secrets. Same posture as `~/.aws/credentials`. |
| Compromised Docker daemon | Docker still materializes libkrun's OCI rootfs and runs OAuth login, so a root-equivalent daemon compromise can tamper with either. Daemonless OCI pull and auth-in-libkrun remain open structural debts. |
| Kernel-level or hypervisor attacks | libkrun/HVF and Cloudflare's isolation reduce the shared-kernel attack surface; compromise of the host kernel, hypervisor, Cloudflare control plane, or VMM remains out of scope. |
| Multi-user separation on a shared host | One secret store per OS user. 0600 blocks other non-root users from reading; a root user on the host bypasses it. |
| Cloudflare/R2 compromise | Managed terminal evidence and encrypted workspace objects leave the local machine. Rustic protects workspace content at rest; the local §0 copy still trusts evidence returned by the managed account boundary. Do not emit provider tokens or unredacted auth responses into evidence. |

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

## Local sandbox isolation in detail

Each normal local `pillbox run` boots a fresh libkrun microVM with:

- a CoW workspace clone mounted at `/workspace/<name>`;
- a per-run CoW credential home, not the user's real `~/.claude`, `~/.codex`,
  or other host configuration;
- an explicit env assembled from `--env`, `--env-file`, `--with`, and pillbox's
  runtime variables rather than the ambient host environment;
- vsock control/attach channels instead of a host daemon API;
- guest networking terminated by the host-owned smoltcp + MITM broker, where
  destination binding, stub swap, and egress policy are enforced;
- a pillbox-owned ingest denylist that excludes credential files, key material,
  `.env` secrets, and other sensitive paths from workspace snapshots.

The OCI runner filesystem is still pulled/unpacked through Docker and cached as
the libkrun rootfs. OAuth login also still runs in a one-shot container. Both are
tracked migration debts, not supported alternative agent placements.

## Managed data boundary

- Pillbox authors no Durable Object state. D1 stores one bounded invocation row,
  R2 stores one immutable terminal artifact, Analytics Engine receives at most
  one compact terminal point, and returned evidence is copied into the caller's
  local session log.
- Public requests carry short-lived controller capabilities scoped to one
  operation and exact resource. They are not participant credentials and cannot
  be replayed for another status read, cancellation, session, or invocation.
- The Worker has no generic Sandbox port/preview proxy; only the named bounded
  execution and workspace routes are public.
- Workspace content is encrypted and content-addressed by rustic in R2. The
  local `repo-password` is not persisted in the session record, but it crosses
  to the managed provision/finalize handler over HTTPS for the foreground
  transfer.
- The host can mint a fresh prefix-scoped R2 credential for provision and
  another for finalize. The bucket-wide parent secret does not cross to
  Cloudflare on that path; the temporary session token is required and
  propagated end-to-end.
- Workspace endpoints are restricted to HTTPS Cloudflare R2 origins. Finalize
  kills prompt-controlled processes before transfer credentials enter the
  Sandbox, and accepts only a canonical 64-hex snapshot id.
- Public managed turns are tool-denied until provider/workspace credentials are
  brokered outside prompt-controlled processes. User-facing capability issuance,
  reconnect semantics, detached finalization, and teardown remain experimental.

## Sessions (detach + reattach)

A detached local session is a long-lived libkrun microVM plus a local session
record. Its credential/egress broker is owned with the VM, so libkrun supports
detached vaulted runs. The deprecated Docker backend does not.

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
the session?" — they would still need access to the libkrun process/socket,
which the host's own permissions gate; the record alone grants nothing.

## Threat model honesty

The biggest residual threat is **the agent exercising an authorized capability
for a malicious instruction**. Pillbox makes raw credential theft harder by:

- keeping real local credentials at the host-owned broker and handing the guest
  destination-bound stubs;
- stripping ambient host env vars;
- supporting default-deny egress with explicit allowlisting;
- excluding secret paths from workspace ingest.

Those controls do not make a permitted API call safe. A prompt-injected agent
may still spend tokens, change code, or send allowed data to an allowlisted
service using the authority the user intentionally granted.

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
