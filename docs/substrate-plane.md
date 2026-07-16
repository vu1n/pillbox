# Substrate plane — one `LiveSession` interface, N substrate handlers

> **Status:** implemented (updated 2026-07-16). `SandboxBackend` +
> `LiveSession` + `Caps` are the shipped plane; libkrun PTY drive/live tail and
> the experimental foreground Cloudflare managed backend are wired through it.
> The phase narrative below is retained as implementation history. Its old
> "Docker as a co-equal local twin" framing is superseded by ADR-001/002:
> libkrun is the one local backend and the Docker agent backend is pending
> deletion.

## Why

pillbox runs an agent on a *substrate*. Today there are two intended placements:
**libkrun** is the default and only supported local agent backend; **managed**
selects the experimental Cloudflare Durable Object + Container path with
`PILLBOX_BACKEND=managed`. The Docker agent backend remains temporarily as
deprecated code and a toolchain-free build fallback; it is not a product mode.

The strategic decision (this is opinionated-for-our-own-use, not
broad-compat):

- **libkrun = the local compute substrate.** It is the default build and owns
  local isolation, PTY handoff, real egress fencing, and in-sandbox grading.
- **Cloudflare = managed placement.** A per-session Durable Object owns §0
  sequencing, actor attestation, arbitration, and fan-out; a Cloudflare
  Container runs the agent. The foreground backend is implemented.
- **Docker agent backend = deprecated.** Docker is still used to materialize the
  libkrun OCI rootfs and for auth plumbing while those dependencies are ported.
  That does not make it a co-equal backend.

### The actual problem to fix

The original problem was scattered backend-string dispatch for `send`,
`attach`, `kill`, live tailing, server HTTP, grading, and ingest. The shipped
fix makes the **live session** polymorphic and gates verbs through explicit
capabilities. `select_backend()` and `live_session()` are the two dispatch
points; command handlers no longer grow backend-specific match arms.

## The real axis: transport *families*, not backends

| Family | Transport | Members | Drive/read mechanics |
|---|---|---|---|
| **Managed container** | HTTP/WS through a per-session DO; container FS is server-side | **Cloudflare Containers/Sandbox** | structured drive + §0 replay/tail |
| **MicroVM** | vsock into an HVF/KVM microVM; FS opaque to the guest | **libkrun** (local) | PTY/server drive + local §0 |
| **Deprecated container** | exec + PTY into a local OCI container | Docker agent backend (pending deletion) | compatibility residue only |

Two more axes are **already abstracted** and stay as-is — orthogonal to this work:

- **Placement** (local file ↔ managed DO): `EventSource` / `sink::EventLog`
  (`src/events/source.rs`, `src/events/sink.rs`).
- **Agent integration** (`Pty` ↔ `Server`): `Integration` + `ServerProfile`
  (`src/agents/mod.rs:52`), already a single source of truth incl. a
  `libkrun_only` capability bit.

We've built this trait-swap pattern three times already (`EventSource`,
`EventLog`, **`SandboxHttp`** at `src/sandbox/http.rs:44` — which is *already*
backend-abstracted: `DockerHttp` + libkrun `opencode_http()` both return
`Box<dyn SandboxHttp>`). This plan extends the same pattern to the one surface
still doing string-match dispatch: the **PTY + lifecycle** surface.

## Contract first (define the boundary before porting)

```rust
// src/sandbox/mod.rs — backend = launch + capability profile; the session is the PLANE.
trait SandboxBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()>;
    fn capabilities(&self) -> Caps;
}
// The single backend-dispatch point: build the plane handle for an EXISTING
// resolved Session (what the command layer calls). A run-path-unifying `start()`
// that launches directly into a LiveSession is deferred — not yet a trait method.
fn live_session(session: &Session) -> Result<Box<dyn LiveSession>>;

// The plane — every command calls these, backend-blind.
trait LiveSession {
    fn caps(&self) -> Caps;
    fn send(&self, bytes: &[u8]) -> Result<()>;                 // PTY/server drive
    fn attach(&self, resolved: &Pillbox) -> Result<()>;          // reattach
    fn event_source(&self)                                       // unifies tailer mess
        -> Result<(Box<dyn EventSource + Send>, Option<TailerHandle>)>;
    fn http(&self) -> Result<Box<dyn SandboxHttp>>;              // server-mode (cap-gated)
    fn workspace_path(&self) -> Result<PathBuf>;
    fn kill(&self, resolved: &Pillbox) -> Result<()>;
    fn score_in_sandbox(&self, /* … */) -> Result<ScoreResult>;  // cap-gated
    fn ingest(&self, resolved: &Pillbox) -> Result<usize>;       // cap-gated
}

// Plain struct, expressible in BOTH builds (docker-only and --features libkrun).
struct Caps {
    pty_drive: bool,          // session send into a PTY
    live_pty_tail: bool,      // live watch/subscribe/wait-idle for a PTY agent
    server_mode: bool,        // Integration::Server agents
    long_lived_exec: bool,    // sandbox spawn/exec/agent
    in_sandbox_grading: bool, // score --in-sandbox + --grader-egress
    real_egress_fence: bool,  // DNS-level allow/deny (not proxy-only)
    detached_vault: bool,     // --detach + --vault together
    post_hoc_ingest: bool,    // ingest (headless capture drain)
}
```

### Scattered symbol → trait method (the port map)

| Current symbol | → `LiveSession` method |
|---|---|
| `sandbox::docker::send_input` / (libkrun: none) | `send` |
| `sandbox::docker::reattach` / `libkrun::session::reattach` | `attach` |
| `sandbox::docker::kill_session` / `libkrun::session::kill_session` | `kill` |
| `spawn_attach_tailer` / `libkrun_server_file_tailer` / `detached_tailer_alive` | `event_source` |
| `http::DockerHttp::new` / `libkrun::session::opencode_http` (`server_http`) | `http` |
| `libkrun::session::score_in_sandbox` (`libkrun_score_in_sandbox`) | `score_in_sandbox` (cap) |
| `libkrun_ingest_events_file` | `ingest` (cap) |
| `libkrun::session::workspace_path` (`libkrun_workspace_path`) | `workspace_path` |

This also deletes the cfg-stub duplication in the command layer (each
`libkrun_*` has a real + a `#[cfg(not(feature="libkrun"))]` stub today — e.g.
`mod.rs:107/111`, `141/173`, `182/191`, `210/226`, `237/249`): the libkrun impl
is gated once, and callers branch on `caps()`, not on the feature.

## Capability matrix (declare, don't chase)

| Verb / capability | docker | libkrun | CF Containers (planned) |
|---|---|---|---|
| run / detach / attach | ✅ | ✅ | ✅ (container family) |
| `send` (PTY drive) | ✅ | ⬜→**fill (Phase 4)** | ✅ |
| live PTY tail (watch/subscribe/wait-idle) | ✅ | ⬜→**fill (Phase 4)** | ✅ |
| server-mode (opencode/codex-serve) | ✅ (opencode) | ✅ | ✅ |
| `sandbox spawn/exec/agent` | ✅ | ❌ | ✅ (CF sandboxes *are* this) |
| `--vault` cred swap | ✅ proxy-only | ✅ in-child | ✅ (proxy/worker) |
| **real egress fence** | ❌ (declare Unsupported) | ✅ | ❌ likely |
| `--detach` + `--vault` | ❌ | ✅ | TBD |
| `score --in-sandbox` | ❌→**port** (`docker run --rm --network none`) | ✅ | ✅ |
| `ingest` | n/a (tails live) | ✅ | n/a (tails live) |

KVM-only isolation (real fence, in-VM MITM) stays **uniquely libkrun** — and CF
Containers will likely decline it too. That asymmetry is correct and permanent;
the plane *exposes* it via `Caps`, it does not chase it.

## Migration order (atomic diffs, port-then-delete, smoke-green each step)

**The spine = Phases 0–2.** Land it first; everything else (default flip,
libkrun PTY parity, CF) builds on it.

- [ ] **Phase 0 — contract.** Add `LiveSession` + `Caps` + `SandboxBackend::start/capabilities`. No behavior change; nothing calls them yet. `cargo fmt --all`, clippy (default **and** `--features libkrun`).
- [ ] **Phase 1 — wrap.** `DockerLiveSession` + (gated) `LibkrunLiveSession` impls that *call the existing free fns*, plus the `live_session(&Session)` factory that constructs the right one. Behavior identical; `scripts/smoke/run.sh` green.
- [ ] **Phase 2 — delete dispatch.** Replace the 8 match sites with `live_session(&session)?` + `caps()` checks, then remove the `Backend::parse` arms:
      `src/commands/session/mod.rs:88,239,576,701,769,993`;
      `src/commands/session/stream.rs:36,56`.
      Move `sandbox spawn/exec/agent` (`src/commands/sandbox.rs:118`, hardcoded `BACKEND_DOCKER`) behind `caps().long_lived_exec`.
      Carry-overs the port must preserve: (1) the `detached_tailer_alive` guard now lives *inside* `LibkrunLiveSession::{event_source,ingest}`, so delete the command-layer copies (`stream.rs:39`, `session_ingest`) rather than double-guarding; (2) `session_subscribe`'s `$PILLBOX_EVENTS_WEBHOOK` exporter — attached today when the tailer `is_some()` (`stream.rs:97`) — must still be wired off `event_source()`'s returned `Option<TailerHandle>`; (3) server-send routing (the http-prompt path that runs *before* the backend match) is orthogonal to `LiveSession::send` (the PTY-drive verb) — a server session must keep going through `http()`, not `.send()`.
      *(End of spine: one plane, dispatch deleted, behavior unchanged.)*
- [ ] **Phase 3 — flip default.** `select_backend()` defaults to libkrun (where available); docker recast in docs/`doctor` as the container-family compat backend, not the default. Rework `doctor` (it probes the Docker daemon + image, `src/doctor.rs:134`) so a libkrun-default host isn't told "Docker not running."
- [ ] **Phase 4 — fill libkrun+PTY drive (in scope; see below).** vsock `send` + live transcript tailing for claude/codex/pi on libkrun, so `Caps{ pty_drive, live_pty_tail }` flip true. This is the IDE/ADE surface — required, not optional.
- [ ] **Phase 5 — later, separate plan.** CF Containers backend = one new `LiveSession` impl in the container family; reuse `SandboxHttp` + `ManagedDoSource`. Out of scope here; the spine makes it a one-handler add.

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
- Deleting docker (it's the container-family reference).
- Touching the §0 contract, `EventSource`, or placement.
- Real egress fencing on docker (declare Unsupported; libkrun owns it).

## Risks

- **libkrun becomes near-sole local substrate** → its host fragility becomes
  load-bearing with no docker fallback. Hardened **opportunistically, in-area**
  alongside the phases — see "Hardening libkrun host fragility" below.
- **Feature gating:** `LiveSession`/`Caps` must compile in a docker-only build;
  only the libkrun impl is `#[cfg(feature="libkrun")]`.
- **Single-writer / seq authority** (the `record_input` dual-writer caveat,
  `mod.rs:799`) is unchanged by this work — note, don't fix here.

## Hardening libkrun host fragility (opportunistic, in-area)

When libkrun becomes the default (Phase 3) it's the near-sole local substrate
with **no docker fallback** — so its known host-fragility failure modes turn
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

**`doctor` becomes backend-aware (Phase 3).** Today it only probes Docker
(`doctor.rs:134-183`) — on a libkrun-default host it would wrongly fail "Docker
not running." Make it report the active backend; for libkrun add: virtualization
present (`/dev/kvm` on Linux, HVF on macOS), runtime deps resolve (#1), disk
headroom (#2), no orphaned VMM groups (#3). Docker demotes to an optional
"compat backend present?" check.

## Verify (every phase)

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features libkrun -- -D warnings
cargo test && cargo test --features libkrun
scripts/smoke/run.sh        # libkrun pre-merge gate (CI is unit-only)
```
