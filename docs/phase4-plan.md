# Plan: substrate-plane Phase 4 (libkrun PTY drive + live read)

Give libkrun PTY agents (claude/codex/pi) the `send` + live-tail surface docker
already has — the keystone for local ADEs/IDEs that drive a session and render
its live §0 stream. Flip libkrun's `pty_drive` + `live_pty_tail` caps true and
fill the two `unsupported` methods. NET-NEW transport (not a refactor), so the
live agent-drive smoke (`scripts/smoke/run.sh`) is the user's gate; I verify
everything non-outward (unit, clippy, `/code-review`, `/vuln-triage`, build,
doctor).

One cohesive task (both halves touch `LibkrunLiveSession` + `capabilities()`).

## P4-001 — libkrun PTY drive (`send`) + live read (`event_source`)

In `src/sandbox/libkrun/session.rs` (+ a host-side send helper):

**send** — replace `LibkrunLiveSession::send`'s `unsupported` with: connect a
`UnixStream` to the persistent attach socket (`LibkrunHandle.sock`, libkrun-bound,
survives the launching CLI), frame the bytes via `attach::driver::send_input`,
drain the pty-host's on-connect `Frame::Snapshot` + a short settle (mirror the
docker `drive_once` settle so the guest forwards the input to the PTY before the
socket closes), close. The detached guest pty-host (`pillbox pty-host
--vsock-listen`, the `run_vsock` accept loop) accepts the transient connection
and writes the `Frame::Input` to the PTY. Flip `caps().pty_drive = true`.

**live read** — replace `event_source`'s PTY-`unsupported` branch with: spawn the
transcript tailer against `LibkrunHandle.creds` (the CoW `creds_share`, a
host-readable virtiofs mount of the agent home — the agent's
`~/.claude/projects/**/*.jsonl` lands there on the host), the same
`spawn_session_observability`/`spawn_attach_tailer` producer the foreground PTY
path already uses (`session.rs:81`), feeding the durable §0 log; the source is
`open_event_source`. Keep the existing `detached_tailer_alive` guard so it
composes with a producer if one exists. Flip `caps().live_pty_tail = true`.

**Known limitations to DOCUMENT (not fix here) — bound the scope honestly:**
1. **Concurrency:** the detached pty-host accept loop is serial (`serve_blocking`,
   `attach/host.rs`). A `send` to an *unattended* detached session (the IDE drive +
   §0-read flow — the keystone) works immediately. A `send` *while a terminal is
   separately attached* queues behind it. The Hub is multi-client; switching that
   loop to threaded `serve` is the follow-up if concurrent drive-while-attached is
   wanted. Note in a code comment + the report.
2. **Multi-reader / unwatched capture:** with no reparented §0 producer for a
   detached PTY session, two concurrent readers (`watch`+`subscribe`) would each
   spawn a tailer → double-write; and a never-watched detached PTY session captures
   nothing. The single-reader IDE keystone is unaffected. The robust fix (a
   reparented PTY transcript producer at `run_detached`, mirroring the server
   `__session-tailer`, so readers follow one producer) is a follow-up — note it.

```yaml
id: P4-001
task_type: feature
depends_on: []
footprint:
  modifies:
    - "src/sandbox/libkrun/session.rs::*"     # LibkrunLiveSession::{send,event_source}, LibkrunBackend::capabilities, a send helper
gate: "cargo clippy (default + --features libkrun) -D warnings clean; full libkrun suite green; a unit test that LibkrunBackend::capabilities().pty_drive && live_pty_tail are now true; build+codesign + `doctor` still report backend=libkrun"
assumptions:
  - "send reuses attach::driver::send_input + a UnixStream to handle.sock; no new frame protocol"
  - "live read reuses the existing creds_share transcript tailer; no new vsock channel"
  - "the two documented limitations are accepted for the keystone; reparented-producer + threaded-accept are follow-ups"
```

## Verification (this phase specifically)

- Non-outward (I run): clippy (both), full suites, `/tighten`, `/code-review` (high),
  **`/vuln-triage`** — the first phase adding a guest input channel (`send` writes
  attacker-influenced bytes to the guest PTY over the host-bound socket), so the
  security pass earns its place: validate the socket is the per-session one, the
  bytes path can't be redirected, no new host-exposure.
- Outward (user runs): `scripts/smoke/run.sh` — boots a VM + drives a PTY agent
  live (billable, needs the runner image + agent auth). The keystone's true e2e
  proof. I'll hand over the exact command + what to look for.

## Follow-ups (surfaced by the Phase 4 review; out of scope for the keystone)

1. **Threaded accept loop** (`attach/host.rs` `run_vsock` `serve_blocking` → `serve`):
   today a `send` *while a terminal is separately attached* may be **dropped**
   (the serial loop won't accept it before the bounded settle tears the socket
   down). The Hub is already multi-client; threading the accept closes it. Needed
   for concurrent drive-while-attached.
2. **Reparented PTY §0 producer** at `run_detached` (mirror the server
   `__session-tailer`): a detached PTY session has no producer, so two concurrent
   readers double-write the log and a never-watched session captures nothing. A
   single reparented producer (readers follow it via `detached_tailer_alive`)
   fixes both — the multiplayer-§0 + unwatched-capture story for PTY.
3. **Pin the symlink-non-following invariant** in `events/transcripts/local.rs`
   `collect_jsonl` (a comment + ideally a `symlink_metadata` containment check):
   the libkrun PTY tailer now reads the guest-writable creds clone, so the
   walker's use of non-following `file_type()` (not `metadata()`) is what stops a
   compromised guest symlinking host files into the §0 log. Safe today; a refactor
   to `metadata()` would silently reintroduce a guest→host escape. (Security
   defense-in-depth — vuln-triage found the channel clean, this hardens against
   regression.)
4. **Bound `pty_send`'s connect/flush** (not just the read): a wedged guest
   pty-host could hang `session send` on connect/write; the docker sibling bounds
   its connect-wait. Batch-loop (eval-harness) hygiene.
