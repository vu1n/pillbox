# Plan: substrate-plane Phase 2 (delete dispatch)

Replace the 8 backend-string dispatch sites with `live_session(&s)?` + `caps()`
checks, delete the now-dead `Backend::parse` arms and `libkrun_*` command-layer
wrappers, and route the sandbox group through `caps().long_lived_exec`. **Strictly
behavior-preserving** — the plane impls (Phase 1) were verified faithful, so this
is a mechanical rewire. The full default (487) + libkrun (517) unit suites must
stay green; any drop is a regression, not a contract change.

This phase is **inherently sequential / non-parallel**: 6 of 8 sites are in one
file and `stream.rs`'s streaming is coupled to `mod.rs`'s helpers. No shared
setup to extract (the plane already exists), so the parallelism-ratio heuristic
doesn't apply — two independent consumption tasks, run in sequence to avoid
working-tree contention.

## P2-001 — Port the session control verbs onto the plane

Rewire `commands/session/mod.rs` + `commands/session/stream.rs`:

| Site | Now | Port to |
|---|---|---|
| `mod.rs:769` send (backend match) | `send_input` / libkrun-reject | `live_session(&s)?.send(text.as_bytes())` (keep the `Integration::Server` http-prompt branch ABOVE it untouched) |
| `mod.rs:576` attach | `reattach`/`libkrun_reattach` | `live_session(&s)?.attach(resolved)` |
| `mod.rs:701` kill | `kill_session`/`libkrun_kill_session` | `live_session(&s)?.kill(resolved)` |
| `mod.rs:88` `server_http` | DockerHttp/libkrun arms | `live_session(&s)?.http()` |
| `mod.rs:239,993` is-libkrun ingest gates | `Backend::parse` match | `live_session(&s)?.ingest(resolved)` + `caps().post_hoc_ingest` where a gate is still needed |
| `stream.rs:36,56` `resolve_streaming_session` | docker/libkrun tailer arms | `live_session(&s)?.event_source(resolved)` |

Then DELETE the now-unreferenced command-layer wrappers + arms: `libkrun_opencode_http`, `libkrun_server_file_tailer`, `libkrun_ingest_events_file`, `libkrun_workspace_path`, `libkrun_reattach`, `libkrun_kill_session`, `server_http`, and the dead `Backend::parse` match arms. KEEP `detached_tailer_alive` (the libkrun impl now calls it) and `tailer_pid`.

**Carry-overs that MUST be preserved (behavior-identical):**
1. Server-send (`Integration::Server`) routing — the http-prompt path that runs BEFORE the backend match — stays in the command layer, unchanged. `LiveSession::send` is the PTY-drive verb only; a server session must NOT go through `.send()`.
2. `session_subscribe`'s `$PILLBOX_EVENTS_WEBHOOK` exporter, attached today when the tailer `is_some()` (`stream.rs:97`), must still be wired off `event_source()`'s returned `Option<TailerHandle>`.
3. The `detached_tailer_alive` double-write guard now lives inside `LibkrunLiveSession::{event_source,ingest}` — so DELETE the command-layer copies (the `stream.rs:39` short-circuit, the `session_ingest` guard) rather than double-guarding.
4. The libkrun-PTY `send` rejection message changes to the plane's standard `unsupported` shape (exit 2 preserved) — acceptable; behavior (error) is identical.

```yaml
id: P2-001
task_type: refactor
depends_on: []
footprint:
  modifies:
    - "src/commands/session/mod.rs::*"
    - "src/commands/session/stream.rs::*"
gate: "cargo clippy --all-targets -- -D warnings AND --features libkrun clean; full `cargo test --bin pillbox` (487) + `--features libkrun` (517) suites stay green; no Backend::parse / libkrun_* wrapper remains in the ported handlers"
assumptions:
  - "the plane impls are faithful (verified Phase 1), so a green suite + a fidelity read per site = behavior preserved"
```

## P2-002 — Route the sandbox group through `caps().long_lived_exec`

`commands/sandbox.rs:118` hardcodes `BACKEND_DOCKER`. Gate it on the backend's
`capabilities().long_lived_exec` instead, so spawning a long-lived exec sandbox
on a backend that lacks it (libkrun today) fails with a clear capability error
rather than silently assuming docker. Behavior on docker is unchanged.

```yaml
id: P2-002
task_type: refactor
depends_on: []
footprint:
  modifies:
    - "src/commands/sandbox.rs::*"
gate: "cargo clippy --all-targets -- -D warnings AND --features libkrun clean; full suites green; sandbox spawn on docker behaves exactly as before; the BACKEND_DOCKER hardcode is replaced by a caps() check"
assumptions:
  - "independent of P2-001 (different file); run after it only to avoid working-tree contention"
```

---

**Validation:** 2 tasks, both consumption-only of the existing plane — no shared
setup exists to sequence, so the ratio heuristic is N/A (documented). Footprints
disjoint (`commands/session/*` vs `commands/sandbox.rs`). Gates are the full test
suites + clippy + a "no dispatch remains" grep. Run sequentially.
