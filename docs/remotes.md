# Remote backends + sessions

> **⚠️ DEPRECATED DIRECTION (2026-06-01).** The whole remote-backend line
> (`ssh://`, `e2b://`, `docker://`) is being retired: "remote" is now
> *Cloudflare-managed* or *pillbox-running-locally-on-the-box*, and the local
> runtime is pivoting Docker → **libkrun microVM**. See
> [libkrun-sandbox.md](./libkrun-sandbox.md) for the direction. The behavior
> below **still ships** (the code is present) but is on the way out — don't
> build new work against it.

> **Describes shipped v0.6 behavior** (`ssh://` + `e2b://`, S3-backed
> workspace), plus `docker://` (parsed, registered, inline; foreground +
> detach + drive/read live-verified). The Docker-context successor model was
> designed in [remotes-redesign.md](./remotes-redesign.md) (now also superseded).

For the command reference, see [../AGENTS.md](../AGENTS.md). This doc
covers the design of the remote-execution path: how pillbox decides
which backend to use, what crosses the wire, and how detached
sessions are tracked.

## The backends, one CLI

| URL scheme | Backend | What it gets you |
|---|---|---|
| `docker://[user@]host[:port]` | `RemoteDockerSandbox` | A remote Docker daemon over SSH transport (`DOCKER_HOST=ssh://…`). The container-is-primitive successor; see [remotes-redesign.md](./remotes-redesign.md). **Status: URL accepted (parse/register/inline); execution path not yet built — `run` errors honestly with the resolved `DOCKER_HOST`.** |
| `ssh://user@host[:port]` | `RemoteSshSandbox` | Your own VPS. Persistent. You administer the host. |
| `e2b://TEMPLATE_ID` | `RemoteE2bSandbox` | E2B managed microVM. Ephemeral. Hardware-isolated. (Deprecated — see redesign.) |

```sh
pillbox remote add prod-vps   ssh://deploy@vps.example.com
pillbox remote add prod-cloud e2b://my-pillbox-template

pillbox run --remote prod-vps                       # interactive, registered
pillbox run --remote prod-cloud --detach --label "nightly"
pillbox run --remote docker://deploy@vps.example    # inline URL, no `remote add`
```

The URL string is the discriminator. `Remote::parsed_url` picks the
backend; everything else (workspace, vault, blob) is identical
across the paths. A `--remote` value containing `://` is treated as an
**inline URL** (no `remote add` needed); anything else is a registered
remote name.

## Distribution requirements

| Backend | Requires |
|---|---|
| docker:// | `docker` + `ssh` on PATH locally; the remote host runs `dockerd` and accepts SSH from you. No remote pillbox install — the runner *image* is pulled by the remote daemon. (Execution path pending; see status note above.) |
| ssh:// | `ssh` on PATH locally (the OpenSSH client). Pillbox itself installed on the remote — we don't deploy binaries. |
| e2b:// | `node` on PATH locally. `npm i -g e2b` (or `bun add -g e2b`). `E2B_API_KEY` exported. Pillbox baked into the E2B template image. |

`pillbox doctor` covers the local Docker prerequisites today; the
`ssh` / `node` / `E2B_API_KEY` checks are added on-demand by the
backends themselves (they fail loudly on first use with an
actionable hint).

## Verifying docker:// (two tiers)

The docker:// path is verified at two levels — both **local / on-demand**, not
GitHub CI (a docker job there burns the free-minutes allocation; the full
agent+vault round-trip also needs `pillbox auth login` creds + a real host):

- **Mechanism (`scripts/test-docker-mechanism.sh`).** Runs the workspace-staging
  + container-lifecycle `#[ignore]` tests against a real daemon using a tiny
  image (`busybox` — they only need `tar`/`sleep`/`test`). Guards the
  **create → stage → start** ordering and the **secret-denylist** on the wire.
  Run locally or on a self-hosted box; point at a remote daemon with
  `DOCKER_HOST=ssh://… scripts/test-docker-mechanism.sh`.
- **Agent + vault (on-demand runbook).** `scripts/verify-remote-docker.sh`
  exercises the real round-trip against a host you provide. It builds the
  from-branch runner image **natively on the target** (the version-skew-safe
  pattern — a laptop-built image is the wrong arch for an amd64 host), runs a
  headless agent, and asserts the agent completed (vault round-trip) and the
  workspace `.env` was excluded (I6). Run it before releases / when the
  docker:// path changes:

  > **Image delivery, BYO.** Building over `DOCKER_HOST=ssh://` is convenient
  > but **buildkit-over-ssh is flaky** (intermittent "no active session" /
  > "context deadline exceeded" — the harness retries). For production BYO,
  > prefer **publishing a prebuilt multi-arch image and `docker pull`ing it on
  > the host**: the host pulls over its datacenter link (fast) and you avoid the
  > ssh-buildkit session entirely. Build-on-target is the dev/branch path. The
  > runner image itself builds fast on a warm daemon — BuildKit cache mounts
  > keep cargo deps compiled, and the in-image binary uses a lighter
  > `runner` profile (no release LTO) — so iteration is cheap once the cache
  > is warm.

  ```sh
  pillbox auth login --agent claude               # once — Tier 3 needs creds
  REMOTE=docker://user@host scripts/verify-remote-docker.sh
  ```

  (Streaming a fast headless agent's *output* awaits the result-capture slice —
  the agent exits before the PTY attach connects; the harness asserts exit
  status + the I6 exclusion note instead.)

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
| `attach` | `--template T --blob-file F --session-id ID [--name N] [--detach]` | Initial run; launches an in-sandbox `pty-host` (which runs the agent under a real PTY) and relays its frames. |
| `reattach` | `--sandbox-id S --session-id ID` | `pillbox session attach <id>`; connects a fresh `pty-relay` to the still-running pty-host (socket derived from the session id). |
| `kill` | `--sandbox-id S` | `pillbox session rm <id>`; calls `sandbox.kill`. |

Pillbox parses a single line of JSON from the helper's stderr — a
`sandbox-up` handshake with `protoVersion` + `sandboxId` — before any
other output. Version mismatches print
`rm ~/.pillbox/cache/e2b-helper-*` as the fix-it. An `attach --detach`
launch then emits a `detached` line; interactive detach (Ctrl-A D /
SIGTERM) is resolved by the host pump, not the helper. Anything else is
passed through to the user's stderr,
sanitized for ANSI/control escapes first.

## Sessions

A session is created when:

1. `pillbox run --detach` succeeds (locally or with `--remote NAME`), OR
2. (Future) the user presses Ctrl-A D during an interactive run — a
   foreground run currently passes Ctrl-A through and has no persisted
   record to leave behind.

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
| Ctrl-A Ctrl-A | Sends a literal Ctrl-A to the session PTY (so readline beginning-of-line still works inside the sandbox). |

Detach can also come from another shell: `pillbox session detach
<id>` reads `attached_pid` from the record and SIGTERMs that
process. The attached pillbox's pump catches SIGTERM (its
detach-enabled handler resolves the session as detached) and exits
cleanly without killing the sandbox.

### Session detach safety

`session detach` validates the target pid before signalling:

- Refuses pid ≤ 1 (init / reserved).
- Refuses self-pid (can't detach from a session you're not attached to).
- Runs `kill(pid, 0)` first — on `ESRCH` (no such process) clears the
  stale stamp and exits without sending SIGTERM, so a recycled pid
  can't get hit when an attached pillbox crashed.

### Backend coverage today

| Operation | local docker | ssh:// | e2b:// |
|---|---|---|---|
| `run` / `run --remote` interactive | ✅ | ✅ | ✅ |
| `run --detach` | ✅ | ✅ | ✅ |
| `session attach` | ✅ | ✅ | ✅ |
| `session detach` | ✅ (kills attached pillbox) | ✅ | ✅ |
| `session rm` | ✅ | ✅ | ✅ |
| `session list` / `info` | ✅ | ✅ | ✅ |

Each backend carries the same attach-transport frames over its own
byte pipe: docker exec stdio (local), ssh stdio (ssh), and an E2B
raw-pty `pty-relay` (e2b). Local `--detach` does NOT support
`--vault` — the host-side stub-swap proxy can't outlive the CLI.

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
