# Substrate plane — one `LiveSession` interface, N substrate handlers

> **Status:** implemented (updated 2026-11-27). `SandboxBackend` +
> `LiveSession` + `Caps` are the shipped plane; libkrun PTY drive/live tail and
> the experimental foreground Cloudflare managed backend are wired through it.
> The agent-run Docker backend is **deleted**: `PILLBOX_BACKEND=docker` is a
> usage error, and old docker records are retired (listable, removable, never
> driven). The phase narrative under "Migration order" is implementation
> history.

## Why

pillbox runs an agent on a *substrate*. There are two placements: **libkrun**
is the default and only local agent backend; **managed** selects the
experimental Cloudflare bounded Worker + Sandbox path with
`PILLBOX_BACKEND=managed`.

- **libkrun = the local compute substrate.** It is the default build and owns
  local isolation, PTY handoff, real egress fencing, and in-sandbox grading.
- **Cloudflare = managed execution placement.** Cloudflare Sandbox's vendor DO
  owns container lifecycle; D1/R2 hold bounded claims/evidence; the caller's
  local `SessionLog` remains §0. The foreground backend is implemented.
- **Docker = runner-image plumbing only.** `src/sandbox/oci.rs` is the one file
  that runs `docker`: it resolves the runner image and exports it as the
  microVM rootfs. It never runs an agent.

### The actual problem it fixed

The original problem was scattered backend-string dispatch for `send`,
`attach`, `kill`, live tailing, server HTTP, grading, and ingest. The shipped
fix makes the **live session** polymorphic, gates verbs through explicit
capabilities, and stores a record's substrate once as a typed value.

## Current shape

- **Selection.** `SubstrateKind::selected()` (`src/sandbox/substrate.rs`) reads
  `PILLBOX_BACKEND` once: unset or `libkrun` → libkrun, `managed` → managed,
  `docker` → usage error (removed), anything else → usage error.
- **One constructor.** `SubstrateKind::backend()` is the only place a substrate
  kind becomes a `Box<dyn SandboxBackend>`. On the build without the `libkrun`
  feature (the in-guest tools build) the libkrun arm is a usage error (exit 2):
  that build has no substrate.
- **Records store it once.** `Session.substrate` and `Sandbox.substrate` hold a
  `Substrate`: `Libkrun(Option<LibkrunHandle>)`, `Managed(ManagedHandle)`, or
  `Retired { backend, handle, why }` for a record left by a removed or unknown
  backend (docker, e2b, …) or with an undecodable handle. On disk it is a
  `[substrate]` table; records written before it migrate on read. The
  `backend` / `placement` / `sandbox_id` output keys are derived from it.
- **Dispatch.** `live_session(&Session)` and `sandbox_target(&Sandbox)` match
  only on `substrate.kind()`: a kind goes through `kind.backend()?`, a retired
  record gets a record-only handle that refuses every verb except
  kill/rm/destroy, which drops the record and says what was left behind (for
  docker: the container id and `docker rm -f <id>`). pillbox never runs
  `docker` for a retired record.
- **Substrate-only operations** that have no session record yet — `auth login`,
  `session score --in-sandbox`, `execution verify-project`, repository
  execution, and `doctor`'s host checks — are `SandboxBackend` methods with an
  unsupported default, so callers reach them through the constructor rather
  than naming the libkrun module.
- **One build boundary.** Only `mod libkrun`, the libkrun arm of
  `SubstrateKind::backend`, the code that names `crate::sandbox::libkrun` (the
  `execution::microvm` drivers, the hidden `text` command, the VMM re-exec
  entrypoints) and libkrun-only tests are gated on the feature — 16 `cfg`
  lines in `src/`. Everything else compiles on every build; the crate-level
  `allow(dead_code)` on the guest build is the only allowance, and CI lints the
  full build on Linux with a type-check-only `cargo clippy`.

## The real axis: transport *families*, not backends

| Family | Transport | Members | Drive/read mechanics |
|---|---|---|---|
| **Managed container** | bounded HTTP execution; container FS is server-side | **Cloudflare Containers/Sandbox** | structured drive + bounded evidence response |
| **MicroVM** | vsock into an HVF/KVM microVM; FS opaque to the guest | **libkrun** (local) | PTY/server drive + local §0 |

Two more axes are **already abstracted** and stay as-is — orthogonal to this work:

- **Event placement:** every backend appends to concrete local `SessionLog`;
  managed evidence is copied there after a bounded execution response.
- **Agent integration** (`Pty` ↔ `Server`): `Integration` + `ServerProfile`
  (`src/agents/mod.rs`), already a single source of truth. Every server agent
  runs on libkrun.

The HTTP seam (`SandboxHttp` at `src/sandbox/http.rs`) is backend-abstracted:
libkrun's `opencode_http()` returns `Box<dyn SandboxHttp>`.

## Contract first (define the boundary before porting)

```rust
// src/sandbox/mod.rs — backend = launch + capability profile; the session is the PLANE.
trait SandboxBackend {
    // Returns what started (Foreground / Detached / ServerReady /
    // StructuredFinished / ManagedFinished) and never prints it; the CLI
    // renders it (commands::run::render_started), dispatch/eval read it typed.
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<Started>;
    fn capabilities(&self) -> Caps;
}
// A new run: SubstrateKind::selected()?.backend()? — the one constructor.
// An EXISTING record: build the plane handle from its stored substrate.
fn live_session(session: &Session) -> Result<Box<dyn LiveSession>>;

// The plane — every command calls these, backend-blind.
trait LiveSession {
    fn caps(&self) -> Caps;
    fn send(&self, bytes: &[u8]) -> Result<()>;                 // PTY/server drive
    fn attach(&self, resolved: &Pillbox) -> Result<()>;          // reattach
    fn spawn_log_tailer(&self, resolved: &Pillbox) -> Result<Option<TailerHandle>>;
    fn http(&self) -> Result<Box<dyn SandboxHttp>>;              // server-mode (cap-gated)
    fn workspace_path(&self) -> Result<PathBuf>;
    fn kill(&self, resolved: &Pillbox) -> Result<()>;
    fn score_in_sandbox(&self, /* … */) -> Result<ScoreResult>;  // cap-gated
    fn ingest(&self, resolved: &Pillbox) -> Result<usize>;       // cap-gated
}

// Plain struct, compiled on every build (with or without the libkrun feature).
struct Caps {
    pty_drive: bool,          // session send into a PTY
    live_pty_tail: bool,      // live watch/subscribe/wait-idle for a PTY agent
    server_mode: bool,        // Integration::Server agents
    long_lived_exec: bool,    // sandbox spawn/exec/agent
    in_sandbox_grading: bool, // score --in-sandbox + --grader-egress
    real_egress_fence: bool,  // DNS-level allow/deny (not proxy-only)
    detached_vault: bool,     // --detach + --vault together
    post_hoc_ingest: bool,    // ingest (headless capture drain)
    repository_execution: bool, // sealed execution/3, exact host-brokered files
}
```

### Scattered symbol → trait method (the port map, executed)

The docker column of this map is gone with the Docker backend.

| Current symbol | → `LiveSession` method |
|---|---|
| `sandbox::docker::send_input` / (libkrun: none) | `send` |
| `sandbox::docker::reattach` / `libkrun::session::reattach` | `attach` |
| `sandbox::docker::kill_session` / `libkrun::session::kill_session` | `kill` |
| `spawn_attach_tailer` / `libkrun_server_file_tailer` / `detached_tailer_alive` | `event_source` |
| `libkrun::session::opencode_http` (`server_http`) | `http` (docker: unsupported) |
| `libkrun::session::score_in_sandbox` (`libkrun_score_in_sandbox`) | `score_in_sandbox` (cap) |
| `libkrun_ingest_events_file` | `ingest` (cap) |
| `libkrun::session::workspace_path` (`libkrun_workspace_path`) | `workspace_path` |

This deleted the cfg-stub duplication in the command layer (each `libkrun_*`
had a real + a `#[cfg(not(feature="libkrun"))]` stub): the libkrun impl is gated
once, and callers branch on `caps()` or reach it through
`SubstrateKind::backend`, not on the feature.

## Capability matrix (declare, don't chase)

| Verb / capability | libkrun | managed (Cloudflare) |
|---|---|---|
| run / detach / attach | ✅ | foreground bounded turn |
| `send` (PTY drive) | ✅ | ❌ (structured drive) |
| live PTY tail (watch/subscribe/wait-idle) | ✅ | ❌ |
| server-mode (opencode/codex-serve) | ✅ | ✅ (one bounded opencode turn) |
| `sandbox spawn/exec/agent/destroy` | ✅ (`exec-host` over vsock, [sandboxes](features/sandboxes.md)) | ❌ (a record can only be destroyed) |
| `--vault` cred swap | ✅ in-child | worker-side |
| **real egress fence** | ✅ | ❌ |
| `--detach` + `--vault` | ✅ | ❌ |
| `score --in-sandbox` | ✅ | ❌ |
| `ingest` | ✅ | n/a (response appended to §0) |
| `--mcp` | ✅ (host loopback forwarded at the gateway; https on 443 joins the allowlist) | ❌ refused |
| sealed repository execution/3 | Implementation in progress; [release gate](local-repository-execution.md) | ❌ |

KVM-only isolation (real fence, in-VM MITM) stays **uniquely libkrun**. That
asymmetry is correct and permanent; the plane *exposes* it via `Caps`, it does
not chase it.

## Migration order (atomic diffs, port-then-delete, smoke-green each step)

**The spine = Phases 0–2.** Land it first; everything else (default flip,
libkrun PTY parity, CF) builds on it.

- [ ] **Phase 0 — contract.** Add `LiveSession` + `Caps` + `SandboxBackend::start/capabilities`. No behavior change; nothing calls them yet. `cargo fmt --all`, clippy (default **and** `--features libkrun`).
- [ ] **Phase 1 — wrap.** `DockerLiveSession` + (gated) `LibkrunLiveSession` impls that *call the existing free fns*, plus the `live_session(&Session)` factory that constructs the right one. Behavior identical; `scripts/smoke/run.sh` green.
- [ ] **Phase 2 — delete dispatch.** Replace the 8 match sites with `live_session(&session)?` + `caps()` checks, then remove the `Backend::parse` arms:
      `src/commands/session/mod.rs:88,239,576,701,769,993`;
      `src/commands/session/stream.rs:36,56`.
      ~~Move `sandbox spawn/exec/agent` behind `caps().long_lived_exec`.~~ Done differently: `commands/sandbox.rs` spawns through `SubstrateKind::selected()?.backend()?.spawn_sandbox` and drives a record through `sandbox_target`; libkrun is the only backend declaring `long_lived_exec`.
      Carry-overs the port must preserve: (1) the `detached_tailer_alive` guard now lives *inside* `LibkrunLiveSession::{event_source,ingest}`, so delete the command-layer copies (`stream.rs:39`, `session_ingest`) rather than double-guarding; (2) `session_subscribe`'s `$PILLBOX_EVENTS_WEBHOOK` exporter — attached today when the tailer `is_some()` (`stream.rs:97`) — must still be wired off `event_source()`'s returned `Option<TailerHandle>`; (3) server-send routing (the http-prompt path that runs *before* the backend match) is orthogonal to `LiveSession::send` (the PTY-drive verb) — a server session must keep going through `http()`, not `.send()`.
      *(End of spine: one plane, dispatch deleted, behavior unchanged.)*
- [x] **Phase 3 — flip default.** Libkrun is the default substrate (the Docker backend was later deleted). Rework `doctor` (it probes the Docker daemon + image, `src/doctor.rs:134`) so a libkrun-default host isn't told "Docker not running."
- [ ] **Phase 4 — fill libkrun+PTY drive (in scope; see below).** vsock `send` + live transcript tailing for claude/codex/pi on libkrun, so `Caps{ pty_drive, live_pty_tail }` flip true. This is the IDE/ADE surface — required, not optional.
- [x] **Phase 5 — experimental.** CF Sandbox backend = one `LiveSession` impl
  using bounded HTTP execution; returned evidence is appended to local §0.

## Phase 4 detail: libkrun + PTY drive (the IDE surface)

**Decided: fill it.** Local ADEs/IDEs build rich interfaces by driving a session
and rendering its live stream — that needs PTY `send` + live tail working on the
local default substrate (libkrun), not just on docker/server-mode.

**Both implementation questions are now answered (code-checked 2026-06-17), and
both resolve the easy way — the mechanisms already exist. Phase 4 is wiring, not
new transport.**

### `send` — feasible, mostly already built

- The attach **`Frame` protocol already carries input**: `Frame::Input(bytes)`
  → writes to the PTY master writer (`src/attach/host.rs:400`).
- A **detached** libkrun PTY session already runs the guest pty-host in
  *listen* mode (`pillbox pty-host --vsock-listen`, `session.rs:470`), accepting
  clients in a loop (`run_vsock`, `host.rs:90-97`); the host-side attach socket
  is libkrun-bound so it **persists after the launching CLI returns**
  (`krun_add_vsock_port2` listen=true) and its path is stored as
  `LibkrunHandle.sock` (`session.rs:1013`).
- So `send` = a small host-side fn (the libkrun analog of
  `sandbox::docker::send_input`): connect to `handle.sock`, do the
  `Hello`/`Snapshot` handshake, write a `Frame::Input`, close. Reuses
  `attach::frame`. ~20 lines.
- **One concurrency note (not a blocker):** the detached accept loop uses
  `serve_blocking` — serial, one client at a time (`host.rs:96`). `send` to an
  *unattended* detached session (the orchestrator/dispatch case) works
  immediately. `send` *while a terminal is separately attached* would queue
  behind it. The Hub is already multi-client (`Vec<Sender>`, Mutex-guarded
  writer); if concurrent drive-while-attached is wanted, switch that loop from
  `serve_blocking` to `serve` (thread-per-client, as the unix `run()` path
  already does). Optional polish — most IDE flows are *attach* (Frame in+out on
  one conn) **or** *send+watch* (no PTY attach held), neither of which needs it.

### live tail — feasible, the tailer already exists

- The PTY agent's **HOME is host-readable**: `GUEST_HOME` (`/home/pillbox`) is a
  **virtiofs share backed by the host dir `creds_share`** (a CoW clone of the
  auth home), mounted by the boot channel (`session.rs:496`,
  `boot::boot_channel(&creds_share, "creds", GUEST_HOME, …)`). virtiofs is
  coherent, so the agent's transcript written in-guest to
  `~/.claude/projects/**/*.jsonl` lands on the host at `creds_share/.claude/…`.
- The **foreground** PTY path *already tails it* into the §0 sink — same
  producer docker uses, no guest emitter:
  `spawn_session_observability(log, …, &launch.creds_share, …)`
  (`session.rs:81-89`).
- `LibkrunHandle.creds` (`session.rs:1015`) persists that host home path for a
  detached session, and `kill_session` only scrubs it on `rm`
  (`session.rs:1285`) — so it's tailable for the whole session life.
- So live tail = replace the "not wired" arm in `resolve_streaming_session`
  (`src/commands/session/stream.rs:69`) with the existing transcript tailer
  pointed at `LibkrunHandle.creds`. **No second vsock, no guest change** — it's
  the same `spawn_attach_tailer`/`spawn_session_observability` docker uses,
  aimed at the host-side virtiofs path.

### The one real design choice left

Single-producer coordination for the detached case: if two readers
(`watch` + `subscribe`) each spawn a tailer they'd double-write the log (the
known dual-writer caveat, `mod.rs:799`). Pick one:
- **(a)** spawn ONE reparented transcript tailer at `run_detached` time (mirrors
  the server path's `__session-tailer`, `session.rs:1054-1077`); readers just
  follow the log. **Recommended** — robust, matches the server pattern.
- **(b)** per-reader tailer guarded by `detached_tailer_alive` (`stream.rs:39`),
  as server-mode does today. Simpler, slightly racier.

Everything lands behind the `LiveSession`/`Caps` boundary from the spine — the
command layer doesn't change, only libkrun's impl flips `pty_drive` +
`live_pty_tail` true and wires these two reuse paths.

## Non-goals (v1)

- Building the CF Containers backend (Phase 5, own plan).
- Touching the §0 contract or event placement.

(Deleting the Docker backend was a v1 non-goal; it was later deleted outright.)

## Risks

- **libkrun is the sole local substrate** → its host fragility is
  load-bearing. Hardened **opportunistically, in-area**
  alongside the phases — see "Hardening libkrun host fragility" below.
- **Feature gating:** `LiveSession`/`Caps` compile on every build; only the
  libkrun impl is `#[cfg(feature="libkrun")]`.
- **Single-writer / seq authority** (the `record_input` dual-writer caveat,
  `mod.rs:799`) is unchanged by this work — note, don't fix here.

## Hardening libkrun host fragility (opportunistic, in-area)

libkrun is the sole local substrate — so its known host-fragility failure modes turn
into "pillbox is broken" for a user. **Do these as you're already editing the
relevant file**, not as a separate phase; each maps to a code area a plane phase
already touches. Guiding principle: **fail loud with a diagnosis + a `Next:`
command — never a silent SIGABRT, a stalled boot, or a `cost==0` record**
(`pillbox run --json` keeps the VMM child's stderr visible for diagnosis).

Already landed (don't redo): `reap_session` + bounded drive/launch (#66);
bring-up-failure kill+reap of the owned VMM child (`session.rs:877`).

Three known failure modes (host-side, **not** pillbox bugs) and where to catch them:

1. **Missing runtime deps → SIGABRT on every launch.** `brew cleanup`/autoremove
   sweeps libkrun's *undeclared* deps (`libepoxy`, `molten-vk`); the VMM child
   then aborts at boot.
   - *Area:* launch path (`session.rs` `prepare_launch` / VMM spawn — Phases 1/4) + `doctor` (Phase 3).
   - *Fix:* map a child that died by `SIGABRT` to an actionable error ("likely missing libkrun deps — `brew install libepoxy molten-vk`"); `doctor` check that the dylibs resolve.
2. **Disk pressure → half-booted VMs stall.** The real H1 culprit (not HVF): a
   CoW clone / rootfs materialize under a full disk hangs the boot.
   - *Area:* preflight in the launch path (before `cow_clone_*` / `materialize_rootfs`) + `doctor`.
   - *Fix:* `statvfs` the cache + clone dir; if headroom < threshold, fail loud with a "free space" `Next:` *before* spawning. `doctor` reports headroom.
3. **Orphaned `__krun-vmm` procs survive teardown.** After churn, reparented VMM
   children outlive `session rm` + `kill -9 <pid>` (pid-only reap misses them) —
   they need a process-group kill.
   - *Area:* `kill_session` + `session prune` (Phases 1/2).
   - *Fix:* launch each VMM child in its own process group (`pre_exec` `setsid`/`setpgid`); `kill_session` kills the **group** (`killpg`), not just the pid; add a sweep that reaps orphaned `__krun-vmm` groups when the session list is empty.

**`doctor` is substrate-aware (done).** It reports the selected substrate and
fails when this build can't run it; the selected backend adds its host checks
(`SandboxBackend::host_checks` — for libkrun: virtualization present, runtime
deps resolve (#1), disk headroom (#2), orphaned VMM count (#3)). Whether the
runner image is available for rootfs export is an optional check.

## Verify (every phase)

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings                       # full build, type-check only
cargo clippy --workspace --no-default-features --all-targets -- -D warnings # in-guest build
cargo test --workspace --no-default-features --all-targets
cargo test --all-targets --features libkrun   # macOS with the libkrun toolchain
scripts/smoke/run.sh        # libkrun pre-merge gate (CI is unit-only)
```
