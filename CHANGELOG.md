# Changelog

All notable changes to pillbox. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/). Section headings track
the design milestone (`v0.6`); the published Cargo crate / runner image
are versioned separately. `0.1.0` is the first non-prerelease crate — it
moves the `:latest` runner image and ships the interactive attach
transport (in-sandbox pty-host + frame protocol; local detach/reattach).
`0.2.0` is the §0 multiplayer trust layer + the libkrun pivot (below).

## Unreleased

- Delete the Docker agent backend. `PILLBOX_BACKEND` accepts `libkrun` (the
  default) and `managed`; `docker` is now a usage error (exit 2) that says the
  backend was removed. Docker stays only as runner-image export plumbing
  (`src/sandbox/oci.rs`, the one file that runs `docker`). The host vault lease
  plane only the Docker backend fed is gone, so `pillbox sidecar` is an
  egress-policy proxy that holds no credentials; the hidden `pty-relay` command
  is removed.
- Session and sandbox records store their substrate once, as a `[substrate]`
  table. Records written before it still load: a docker or other removed
  backend's record is listed and can be removed (pillbox prints the container it
  left behind and never runs `docker` for it), but every other verb refuses it.
  JSON output keeps the `backend`, `placement` and `sandbox_id` keys.
- `--mcp`/`--mcp-token` work on the libkrun microVM for claude, codex and
  opencode: an `http://` server on the host's loopback (`localhost` /
  `127.0.0.1`) is forwarded to the guest at the gateway, and a remote server must
  be `https://<dns-name>` on port 443 (its name joins the egress allowlist).
  `https://` or `[::1]` on loopback is refused. Other agents and the managed tier
  refuse `--mcp` instead of ignoring it.
- **Downgrade breaks record listing:** a session or sandbox record written by
  this version (any new record, or an old one this version rewrote) can't be
  read by an older pillbox, so its `session list` / `sandbox list` fail until
  those records are removed. A record whose `[substrate]` kind this version
  doesn't know (written by a newer pillbox) is listed and removable, and is
  written back unchanged; handle fields it doesn't know are kept too.
- An unrecognized `PILLBOX_BACKEND` value now exits 2 instead of being ignored;
  the valid values are unset, `libkrun` and `managed`.
- A legacy docker record reports `placement: null` in JSON output.
- JSON `sandbox_id` for a pre-existing record is re-serialized from the decoded handle: a managed handle stored with `do_session_id` reports `execution_session_id`, and a libkrun handle stored with `"rootfs":null` reports no `rootfs` key.
- `session info` text shows one `substrate:` line instead of `backend:` and
  `sandbox_id:` lines.
- `pillbox sidecar` no longer answers 401 to bearer requests on provider hosts:
  it holds no credentials and forwards them; provider OAuth token endpoints stay
  refused (403).
- `run --mount` and a preset `mount` are refused (exit 2) before anything
  launches, on every caller (CLI, presets, dispatch, eval): the microVM takes no
  extra host mounts, and since the Docker backend went nothing honored them.
- A pillbox built without the `libkrun` feature is the in-guest tools build with
  no substrate: `run`, `auth login`, `session score --in-sandbox`, `execution
  verify-project`, `dispatch` and `eval` refuse with exit 2. `execution
  verify-project` is now listed in that build's help (it refuses there).
- CI lints the full (libkrun) build on Linux with a type-check-only `cargo
  clippy`; the feature gate covers only code that names the libkrun module (16
  `cfg` lines, down from 93).

- The session packet (`pillbox.session-packet/1`) is now its own workspace
  crate, `pillbox-packet` (`crates/packet/`), with a standalone
  `pillbox-packet pack|unpack` binary that builds without the rest of pillbox
  (`cargo build -p pillbox-packet`). Its flags, JSON, exit codes and packets are
  unchanged and identical to `pillbox checkpoint pack|unpack`, which stays. The
  format spec and schema moved beside the crate
  (`crates/packet/session-packet.md`, `crates/packet/session-packet-1.schema.json`).
  Credential file names the packet excludes and the agent registry uses now come
  from one table, the `pillbox-credentials` crate (`crates/credentials/`).
- Remove the managed tier's private Workers RPC entry point
  (`HuddlesRuntimeEntrypoint`), its Ed25519 operation-grant protocol, the
  authorization-currentness service binding, the Huddles owner domain, and the
  manual Huddles-recorded burn-in (bootstrap validator, fixed-workload recorder,
  reconciler, cost receipt and cost accounting scripts, `wrangler.burnin.toml`,
  `docs/burn-in/`). The Worker serves only the public HTTP routes, authorized by
  short-lived HMAC capabilities; the owner is always `public_controller`, and
  `tool_policy` must be `deny_all` (HTTP 400 `unsupported_policy` otherwise). The
  D1 allowance is the cost breaker. A managed preview deployment now needs a new
  Pillbox-recorded burn-in, which does not exist yet.
- Managed execution no longer admits the execution transport label
  `cloudflare-service-binding`, which described only the deleted private RPC
  caller. A public request carrying it now gets `unsupported_execution`, and no
  in-repo client sends it.
- Rename `docs/huddles-codex-execution.md` to `docs/codex-execution.md`.
- `pillbox dispatch` grades every worker on a copy of its workspace inside the
  grader microVM, never on the host and never in the live tree. Tests, test
  config, CI config and rubrics are reset to the bookmark and locked, and the
  grader runs as `nobody` (`--protect`, `--no-default-protect`,
  `--grader-egress`). `--grader-files DIR` injects hidden checks at
  `.pillbox-grader/`. A grader tool missing from the runner image makes the
  worker `errored`, not `failed`. A build without libkrun refuses to dispatch.
- Add `pillbox dispatch --restate-task` (off by default). It re-sends the
  positional task, verbatim in an `<original-task>` block, with every retry
  turn and ahead of every segment after the first. If the task already contains
  those tags, the tag becomes `<original-task-N>` so the whole task stays inside
  the block. A segment retry also restates that segment's prompt in a
  `<current-segment>` block. Without the flag a retry carries only the failure
  summary and only segment 1 sees the task.
- Remove the `pillbox session started` command. Nothing in the tree called it;
  `session done` is unchanged.
- Remove the unused ACP adapter spikes and the local-test-only legacy Huddles
  `ensureSession`/`invokeSession` adapter. Managed Huddles execution uses only
  the authenticated `pillbox.execution/2` operations, as before.

## v0.3.0 — 2026-10-04

- Isolate each local VM and grader in a private writable rootfs. Exclude old
  shared caches from new launches; cloning failures stop launch. Cleanup waits
  for confirmed shutdown and handles restrictive directory modes without
  following symlinks. Existing VMs need restarting to acquire isolation.
- Stop structured VMM process groups when their CLI owner dies, including
  SIGKILL, while preserving intentional detached/server lifetimes.
- Preserve the host project and cwd for implicit bookmark forks. Restoring into
  a host directory requires explicit `--workspace` intent.
- Add sealed, bounded local repository execution, tool-free text invocation,
  and repository text edits for Huddles self-build.
- Refresh all seven bundled harnesses to the stable versions recorded in
  [runner-image.md](docs/runner-image.md). Sealed Codex profiles remain pinned
  to their separately qualified versions.
- Add the optional DigitalOcean managed execution runtime.

### Additional changes since v0.2.0

These notes record earlier iterations. Current managed authority and preview
release gates are defined in [managed-tier.md](docs/managed-tier.md) and
[durable-object-usage.md](docs/durable-object-usage.md).

- Added `[preset.NAME]` to `pillbox.toml` and `pillbox run --preset NAME`: a
  named, reusable run environment (agent, model, temperature, mounts, secret
  and env-bundle names, MCP servers, the egress allowlist and default-deny)
  declared once in the descriptor and cascading global→project by name.
  Explicit flags still win (lists union, scalars override, booleans or); the
  applied preset prints one stderr line. Re-fixes the v0.5 run-defaults v0.6
  dropped, now under a name rather than at the top level.
- Added `conditions[]` to `session list|info|diagnose --json` and a conditions
  table to the human `diagnose` readout: `Ready`, `AwaitingInput`, `Finished`,
  `ResultAvailable`, each True/False with a reason, message, and the
  transition time when the log carries one (the Kubernetes condition shape).
  `session.completed`/`failed` lifecycle lines now surface their `ended_at`.
- Added `scripts/demo.sh`: the full local lifecycle in one scripted run —
  doctor, new, run, send, wait-idle, diagnose, log, pull, score — printing each
  command as you would type it.
- README opens with a "You want to… / pillbox gives you" table;
  `docs/agent-io-contract.md` gains a "what an emitter must do" contract +
  checklist.

- Added the experimental Cloudflare `ManagedBackend`: rustic-on-R2 workspace
  provision, structured agent drive through the per-session Durable Object,
  wait-for-idle, finalize, and result-snapshot recording.
- Added prefix-scoped R2 temporary credentials and end-to-end session-token
  propagation through the DO into the container workspace helper.
- Live-validated the current scoped credential path on a paid Cloudflare
  Container: encrypted R2 snapshot, provision, real agent edit, finalize, and
  ordinary result pull all completed end to end.
- Added Cloudflare gateway CI covering TypeScript, Rust↔TypeScript contract
  parity, actor authentication, driver arbitration, annotations, and real
  workerd/Durable Object SQLite.
- Reframed the public surface around Durable Objects as the managed session
  authority, local libkrun handoffs as the superpower, and Ghost as the
  experimental proof tenant.
- Added contributor guidance and the declared MIT/Apache-2.0 license files.

## v0.2.0 (crate) — local-only (libkrun pivot) + §0 multiplayer

> The crate version (`0.2.0`, what `pillbox version` reports) and the design
> milestone (`v0.6`) are separate namespaces. This section is the post-`v0.6`
> work; the `v0.6 PR` entries below are kept as history.

### remote plane removed → local-only

- **Removed the entire remote backend plane:** the `remote add/list/info/rm`
  commands, `pillbox run --remote`, the `ssh://` / `e2b://` / `docker://` URL
  backends, `RemoteSsh` / `RemoteE2b`, and the e2b Node helper + `pty-relay`
  bridge. pillbox is now **local-only**. This retires the v0.6 PR 4/5 and the
  ssh/e2b halves of the interactive-attach-transport entry below. "Remote"
  returns later as a managed/Cloudflare tier with a different shape.

### libkrun backend (local microVM)

- New **libkrun** backend, opt-in via `PILLBOX_BACKEND=libkrun`: a local microVM
  (macOS/HVF, Linux/KVM), no daemon. Owns the VMM via FFI; vsock control plane +
  smoltcp userspace egress; an in-guest `pillbox-init` runs the pty-host + frame
  protocol. Boot via a creds-share boot script (off the ASCII-only kernel
  cmdline). Full session surface incl. detach/reattach and vaulted detached runs.
- Rootfs cache keyed by docker **image id** (not tag); falls back to a cached
  rootfs when docker is unreachable.

### §0 multiplayer trust layer

- Every §0 event now carries a producer-stamped **authenticated actor**.
- `pillbox session send` = durable, attributed input (the drive half);
  `pillbox session annotate ID TEXT [--anchor REF]` = a non-driving attributed
  §0 comment (multiplayer "chime in").
- §0 log append is **locked** so concurrent writers can't collide on `seq`.
- `pillbox session watch` renders actor attribution + Input/Annotation.
- Cloudflare Durable-Object-as-§0-gateway **spike** (contract.ts ↔ contract.rs
  parity, machine-checked) — the managed-tier direction; a spike, not product.

### credential vault — policy-bound egress broker

- **Default-deny egress broker:** `pillbox run --vault --egress-deny` blocks any
  host with no credential provider that isn't on `--egress-allow`.
- `--egress-allow HOST` (repeatable; exact or `.suffix`) opens specific hosts
  through both the libkrun egress fence and the broker allowlist.
- Destination-bound StubSwap; **per-run ephemeral CA** by default (stable CA
  opt-in via `pillbox vault ca`); SSRF / DNS-rebind guard on the forward leg.

### sessions — the optimization & read surface

- `pillbox session score` — external grading and the **verifiable reward
  channel**: `--cmd` (one verifier) | `--rubric FILE` (per-criterion verdicts +
  fractional score); `--snapshot`/`--workspace`; `--in-sandbox` one-shot microVM
  grader; `--grader-egress HOST`; `--json` verdict.
- `pillbox session ingest` — drain the durable §0 capture into `log.jsonl`,
  post-hoc + idempotent (libkrun opencode).
- `pillbox session log` — structured read of `log.jsonl` (`--type`/`--last`/`--from`).
- `pillbox session wait-idle` — block until the turn goes idle (`--timeout`/`--from`).
- `pillbox session diagnose` — derived status + activity summary.
- `pillbox session prune [--dry-run]`, `session pull`; `run --ttl` / `--parent`.

### agents & integration

- **opencode** runs server-mode (`opencode serve` + `/event` SSE + `/prompt`) on
  Docker **and** libkrun (SandboxHttp seam + vsock-forward). `codex-serve` drives
  `codex app-server` (JSON-RPC), libkrun-only, sharing `codex` auth.
- `--model PROVIDER/MODEL` + `--temperature FLOAT` for server-mode agents.
- `--mcp NAME=URL` shared-MCP attach + `--mcp-token NAME=SECRET_NAME` (bearer
  token from the secret store; never in argv/history).
- `--memory` wires in the external [`kypp`](https://github.com/vu1n/kypp)
  swarm-memory engine (brief-before / capture-after, host-side, best-effort).
- runner image bumps: codex `0.139.0`, opencode `1.17.3`, pi `0.79.1`.

### observability

- `pillbox session transcript --follow` emits OTLP child spans from agent-native
  transcripts; sandbox startup timings instrumented.

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
