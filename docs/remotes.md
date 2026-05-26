# Remote backends + sessions

For the command reference, see [../AGENTS.md](../AGENTS.md). This doc
covers the design of the remote-execution path: how pillbox decides
which backend to use, what crosses the wire, and how detached
sessions are tracked.

## Two backends, one CLI

| URL scheme | Backend | What it gets you |
|---|---|---|
| `ssh://user@host[:port]` | `RemoteSshSandbox` | Your own VPS. Persistent. You administer the host. |
| `e2b://TEMPLATE_ID` | `RemoteE2bSandbox` | E2B managed microVM. Ephemeral. Hardware-isolated. |

```sh
pillbox remote add prod-vps   ssh://deploy@vps.example.com
pillbox remote add prod-cloud e2b://my-pillbox-template

pillbox run --remote prod-vps           # interactive
pillbox run --remote prod-cloud --detach --label "nightly"
```

The URL string is the discriminator. `Remote::parsed_url` picks the
backend; everything else (workspace, vault, blob) is identical
across the two paths.

## Distribution requirements

| Backend | Requires |
|---|---|
| ssh:// | `ssh` on PATH locally (the OpenSSH client). Pillbox itself installed on the remote — we don't deploy binaries. |
| e2b:// | `node` on PATH locally. `npm i -g e2b` (or `bun add -g e2b`). `E2B_API_KEY` exported. Pillbox baked into the E2B template image. |

`pillbox doctor` covers the local Docker prerequisites today; the
`ssh` / `node` / `E2B_API_KEY` checks are added on-demand by the
backends themselves (they fail loudly on first use with an
actionable hint).

## What crosses the wire

Both backends use the same internal "vault-stdin blob" shape:

```jsonc
{
  "version": 2,
  "agent_id": "claude",
  "agent_args": ["--continue"],
  "workspace_mount_name": "my-app",
  "vault": true,
  "workspace": {
    "endpoint": "https://acct.r2.cloudflarestorage.com",
    "region": "auto",
    "bucket": "my-snapshots",
    "prefix": "pillbox/",
    "access_key": "<resolved value>",
    "secret_key": "<resolved value>",
    "repo_password": "<rustic repo password>",
    "base_snapshot": "<64-char handle>"
  },
  "secrets": [
    { "name": "ANTHROPIC_API_KEY",
      "env_var": "ANTHROPIC_API_KEY",
      "value": "<real plaintext>",
      "vault_meta": null }
  ],
  "env": { "KEY": "value" }
}
```

- SSH: blob is fed over `ssh`'s stdin (encrypted channel) into
  `pillbox run --vault-stdin` on the remote. The blob itself is not
  persisted; the remote writes the repo password to a 0600 temp file
  while hydrating/pushing the workspace.
- E2B: blob is uploaded to the sandbox's `/tmp` via the E2B Files API
  (mode 600), unlinked by the launch line as soon as the in-sandbox
  pillbox reads it. The local pillbox stages a 0600 tempfile to pass
  the bytes to the helper subprocess (atomic O_EXCL via `tempfile`),
  also unlinked on exit.

The blob format is versioned and shared between backends. A mismatch
(`version != BLOB_VERSION`) fails the parse loudly so a newer client
paired with an older remote can't silently drop required fields.
`Debug` is implemented by hand on `VaultStdinBlob`, `InlineSecret`,
and `InlineWorkspace` so a stray `dbg!` or `tracing::debug!(?blob)`
never leaks secret material to logs.

## Workspace handoff

v0.6 supports remote runs only against an S3-shaped workspace backend.
At launch, the local side either snapshots the current workspace or
resolves `--from-bookmark NAME`, then sends that base snapshot plus
the S3/R2 repo coordinates and repo password in the blob. The remote
side restores the base into an isolated temp workspace before running
Docker, then pushes the result workspace back to the same repo after
the agent exits.

Both sides use `rustic_core` against the same repo. The durable
encryption password stays in the local pillbox state
(`<state_dir>/repo-password`, 0600); remote runners receive it only as
per-run material.

A local-rustic workspace errors out with an actionable pointer
(`pillbox new --workspace-backend s3 …`). Tarball transport over the
wire is the planned PR 4.1 follow-up.

## E2B helper subprocess

There's no usable Rust SDK for E2B (the only third-party crate is
code-interpreter only — no PTY, no `commands.run`). Pillbox embeds a
small Node helper script (`src/sandbox/e2b-helper.mjs`) via Rust's
`include_str!`, writes it to a versioned cache path on first use
(`~/.pillbox/cache/e2b-helper-vX.mjs`, 0700), and spawns
`node helper.mjs <mode> …` as a subprocess.

The helper has three modes:

| Mode | Args | Used for |
|---|---|---|
| `attach` | `--template T --blob-file F [--name N] [--detach]` | Initial run; creates sandbox + PTY, launches agent. |
| `reattach` | `--sandbox-id S --pid P` | `pillbox session attach <id>`; calls `sandbox.pty.connect`. |
| `kill` | `--sandbox-id S` | `pillbox session rm <id>`; calls `sandbox.kill`. |

Pillbox parses a single line of JSON from the helper's stderr — a
`sandbox-up` handshake with `protoVersion`, `sandboxId`, `pid` —
before any other output. Version mismatches print
`rm ~/.pillbox/cache/e2b-helper-*` as the fix-it. Subsequent JSON
event lines (`detached`, `detach-pressed`) drive the session
lifecycle; anything else is passed through to the user's stderr,
sanitized for ANSI/control escapes first.

## Sessions

A session is created when:

1. `pillbox run --remote NAME --detach` succeeds, OR
2. (Future) the user presses Ctrl-A D during an interactive remote run.

The record lives at `<pillbox>/sessions/<id>.toml`. ID is 12 hex
chars (48 bits). Per-pillbox, no inheritance — a session is concrete
runtime state tied to where it was started, not a config.

```toml
id = "abc123def456"
label = "nightly"               # optional
remote = "prod-cloud"
backend = "e2b"
sandbox_id = "sb_xxx"
pty_pid = 42
agent_id = "claude"
started_at = "2026-05-21T13:37:00Z"
attached_pid = 12345            # PID of an attached pillbox, or absent if detached
```

### Detach hotkey

When attached, **Ctrl-A** is the prefix:

| Sequence | Effect |
|---|---|
| Ctrl-A D | Detach from the session. Sandbox keeps running. |
| Ctrl-A Ctrl-A | Sends a literal Ctrl-A to the remote PTY (so readline beginning-of-line still works inside the sandbox). |

Detach can also come from another shell: `pillbox session detach
<id>` reads `attached_pid` from the record and SIGTERMs that
process. The attached pillbox catches SIGTERM via the helper's
`detach-pressed` path and exits cleanly without killing the sandbox.

### Session detach safety

`session detach` validates the target pid before signalling:

- Refuses pid ≤ 1 (init / reserved).
- Refuses self-pid (can't detach from a session you're not attached to).
- Runs `kill(pid, 0)` first — on `ESRCH` (no such process) clears the
  stale stamp and exits without sending SIGTERM, so a recycled pid
  can't get hit when an attached pillbox crashed.

### Backend coverage today

| Operation | ssh:// | e2b:// |
|---|---|---|
| `run --remote` interactive | ✅ | ✅ |
| `run --remote --detach` | ❌ (not yet) | ✅ |
| `session attach` | ❌ (not yet) | ✅ |
| `session detach` | ✅ (kills attached pillbox) | ✅ |
| `session rm` | ❌ (not yet) | ✅ |
| `session list` / `info` | ✅ (records, if any) | ✅ |

SSH session persistence needs a tmux-on-the-remote integration. Today
the SSH backend errors loudly on `--detach` with the not-yet-
implemented hint; landing in a follow-up.

## Forward compatibility

- **Blob version** (`BLOB_VERSION`) is enforced. Bumped only for
  semantic-breaking changes; unknown JSON keys within a known version
  are still tolerated (serde default).
- **Helper proto version** (`HELPER_PROTO_VERSION`) is enforced. The
  Rust side fails fast and prints a `rm ~/.pillbox/cache/e2b-helper-*`
  fix-it if it sees a stale extracted helper.
- **Unknown helper event types** are forwarded to stderr (sanitized)
  rather than swallowed, so a future helper that adds events stays
  visible to older pillboxes as raw diagnostics.

## See also

- [security.md](./security.md) — threat model including remote
  backends and the vault-stdin handoff.
- [../AGENTS.md](../AGENTS.md) — agent-facing command reference.
