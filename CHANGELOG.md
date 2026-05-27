# Changelog

All notable changes to pillbox. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/). Section headings track
the design milestone (`v0.6`); the published Cargo crate / runner image
are versioned separately. `0.1.0` is the first non-prerelease crate — it
moves the `:latest` runner image and ships the interactive attach
transport (in-sandbox pty-host + frame protocol; local detach/reattach).

## v0.6 — pillbox-as-bundle + remote backends

The v0.6 reshape: a pillbox is a self-contained bundle of (workspace
+ code + vault + config) the user creates, lists, runs, and
removes. Per-project state, shared agent auth, and a path to remote
execution that travels with the pillbox.

### interactive attach transport (in-sandbox pty-host + frame protocol)

- The agent now runs under an in-sandbox `pillbox pty-host` (owns the
  PTY + a `vt100` screen model, serves a binary frame protocol on a unix
  socket) across ALL backends: local Docker, ssh, and e2b. Attaching
  speaks the same `frame.rs` codec over each backend's byte pipe —
  `docker exec` stdio, ssh stdio, and (e2b) a raw-pty `pty-relay`
  bridged through the Node helper's stdio.
- Attach replays a bounded ANSI **snapshot** of current screen state, so
  a fresh client repaints without a full-history replay.
- The host-side `attach::pump` owns raw mode, resize, and the Ctrl-A D
  detach for every backend; the e2b Node helper shrank to a dumb byte
  shuttle (no more cooked-text streaming or Ctrl-A handling). e2b
  reattach derives the pty-host socket from the session id (no `--pid`).
- Detach is session-only: a foreground `run` passes Ctrl-A through and
  has no destructive detach.

### v0.6 PR 7 — polish + docs

- Full README rewrite reflecting the post-PR-6 surface.
- New [docs/remotes.md](./docs/remotes.md) covering both backends +
  sessions + the helper-subprocess protocol.
- [docs/security.md](./docs/security.md) gains a "Remote backends"
  section: vault-stdin blob, helper-subprocess threat surface,
  Ctrl-A D + session detach safety.
- [docs/recipes.md](./docs/recipes.md) gains four new recipes (VPS,
  E2B, detached session, workspace snapshot/restore).
- New top-level [SECURITY.md](./SECURITY.md) for GitHub's
  vulnerability-reporting UI.

### v0.6 PR 6 — sessions (list / attach / detach)

- `pillbox run --remote NAME --detach [--label TEXT]` starts the
  session in the background; reattach with `pillbox session
  attach <id>`.
- Full `pillbox session list | info | attach | detach | rm` surface.
- Detach hotkey: Ctrl-A D (Ctrl-A Ctrl-A sends a literal Ctrl-A so
  shell readline still works inside the sandbox). Also detachable
  from another shell via `pillbox session detach <id>`.
- Helper grew `reattach` and `kill --sandbox-id S` modes (the e2b
  attach wire was later reshaped onto the frame protocol — see the
  interactive-attach-transport entry above).
- `session_detach` SIGTERM is guarded against pid reuse + reserved
  pids + self-pid (kill(pid, 0) liveness probe first).
- ANSI-escape sanitizer on every helper-stderr passthrough.
- Cross-PR cleanup: `build_vault_stdin_blob` extracted into
  `remote_ssh.rs` and shared by both backends (~130 LOC dedup).

### v0.6 PR 5 — RemoteE2b backend

- `e2b://TEMPLATE_ID` URL scheme on the remote registry. `Remote`
  shape unchanged — `url` is the discriminator; added
  `RemoteUrl::E2b(E2bRef)` and `Remote::parsed_url()`.
- New `RemoteE2bSandbox` backend. Reuses `VaultStdinBlob` /
  `BLOB_VERSION` from PR 4.
- Bridges to E2B via a small embedded Node helper
  (`src/sandbox/e2b-helper.mjs`, written to a versioned cache path
  on first use). No usable Rust SDK today.
- Helper ↔ Rust handshake (`{type:"sandbox-up", protoVersion, …}`)
  is parsed + version-checked; mismatched cached helpers get an
  actionable `rm ~/.pillbox/cache/e2b-helper-*` hint.
- 0600 temp blob staged via `tempfile`'s atomic `O_EXCL`.
- `~/.pillbox/cache/` is 0700, matching every other pillbox-owned
  dir.

### v0.6 PR 4 — RemoteSsh backend (`pillbox remote`)

- `pillbox remote add NAME URL [--agent A] [--global]` registry
  with `list / info / rm`. Per-pillbox TOML at
  `<pillbox>/remotes/<name>.toml`, global + project scopes.
- `pillbox run --remote NAME` dispatches through
  `SandboxBackend::RemoteSsh`. Vault material crosses the network
  exactly once as a versioned JSON blob piped on SSH stdin to a
  hidden `--vault-stdin` handler on the remote side.
- `VaultStdinBlob::version` enforced; `serde_json` parse errors
  remapped to a fixed string (no stdin-byte leakage on stderr);
  hand-written `Debug` redacts secret values; no
  `StrictHostKeyChecking=no` anywhere.

### v0.6 PR 3 — workspace versioning via `rustic_core`

- Every project pillbox owns one rustic repository: encrypted,
  content-addressed, deduplicated.
- `pillbox push / pull / snapshot list/show/rm / workspace rekey`.
- Backends: `local` (default, `<state_dir>/repo/`) and `s3`
  (S3-compatible — R2, MinIO, Backblaze) via `opendal`.
- Encryption password lives at `<state_dir>/repo-password` (0600,
  local-only). Stolen bucket alone can't be decrypted.
- `pillbox new --from-git URL` inflow.

### v0.6 PR 2 — pillbox-as-bundle core (CLI redesign)

- `init / new / list / rm / info` — pillbox lifecycle.
- `--pillbox NAME` global flag overrides cwd-based discovery.
- `pillbox.toml` becomes the descriptor; `meta.json` mirrors it
  in the state dir.
- Path-encoded keys: `/Users/vuln/work/foo` →
  `-Users-vuln-work-foo`.
- v0.5 → v0.6 is a hard reset; no migration shim.

### v0.6 PR 1 — `SandboxBackend` trait + sidecar mode

- Run path split off `AgentSpec` into a trait; the dispatcher
  picks at runtime (`LocalDocker` first; `RemoteSsh` + `RemoteE2b`
  land in later PRs).
- `pillbox sidecar` runs the credential vault as a standalone
  process for remote runs.
- `--strict` (v0.5) removed in favor of the proxy-default posture.

## v0.5 — credential vault + provider integration

- `--vault` flag wraps agent traffic through an in-process MITM
  proxy that swaps stub credentials for real ones at the network
  boundary.
- Providers: Anthropic OAuth, Codex ChatGPT OAuth, user-level API
  keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`).
- GitHub Actions CI (ubuntu + macos matrix).

## v0.4

- First credential-vault iteration (Anthropic only).

## v0.1–v0.3

- Initial Claude / Codex sandboxing, secrets + env bundles,
  `pillbox.toml` v1.
