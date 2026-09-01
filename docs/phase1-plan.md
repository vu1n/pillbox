# Plan: substrate-plane Phase 1 (wrap)

Wrap the existing per-backend free functions behind the Phase-0 `LiveSession`
trait, and add the **single** construct-from-session factory the command layer
will call instead of the 8 scattered `Backend::parse` matches. **No behavior
change** — nothing calls the new code yet (it's consumed in Phase 2), so
`#[allow(dead_code)]` is expected until then.

**Reshape from the doc (deliberate):** the doc's Phase 1 says "`start()`
constructs the right one." But every dispatch site Phase 2 deletes operates on
an *existing resolved `Session`* (a control verb on a running session), not on a
launch. So the factory is `live_session(resolved, &Session)`, and `start()`
(provision/launch unification) stays the Phase-0 bailing default — deferred to a
later step. Reconcile docs/substrate-plane.md Phase 1 wording accordingly.

Critical path: LSW-001 → (LSW-002 ‖ LSW-003) → LSW-004.

## LSW-001 — Shared `Caps::unsupported(verb)` error helper

Both `LiveSession` impls must reject a verb the backend can't do (libkrun PTY
`send`/`event_source`; docker `ingest`) with one consistent shape — verb name +
the standard `pillbox: … Next:` form — instead of a bespoke `bail!` per impl.
Lands first because both impls call it.

```yaml
id: LSW-001
task_type: feature
archetype: backend
depends_on: []
footprint:
  modifies:
    - "src/sandbox/mod.rs::Caps"   # add inherent `unsupported(&self, verb) -> anyhow::Error`
gate: "cargo clippy --all-targets -- -D warnings clean; unit test asserts the error string names the verb"
assumptions:
  - "helper hangs off Caps; if it reads better as a free fn in sandbox::mod, that's an in-footprint choice"
```

## LSW-002 — `DockerLiveSession` + `impl LiveSession`

Wrap the existing docker free fns: `send_input`→`send`, `reattach`→`attach`,
`kill_session`→`kill`, `http::DockerHttp`→`http`, `spawn_attach_tailer` (open
`SessionLog` + tail the bind-mounted agent home)→`event_source`,
`workspace_path`→`workspace_path`. `ingest` → `Caps::unsupported` (docker drains
live). `caps()` returns `DockerBackend::capabilities()`. Holds the `Session`
record; methods take `resolved` where the trait already passes it.

```yaml
id: LSW-002
task_type: feature
archetype: backend
depends_on: ["LSW-001"]
footprint:
  modifies:
    - "src/sandbox/docker.rs::DockerLiveSession"   # new struct + impl LiveSession (file exists)
gate: "cargo clippy --all-targets -- -D warnings clean; cargo test --bin pillbox green; unit test: a DockerLiveSession reports caps().pty_drive==true and ingest() returns the unsupported error"
assumptions:
  - "struct holds a cloned Session (+ container id from it); if a verb needs more, it stays inside this footprint"
  - "#[allow(dead_code)] until Phase 2 wires the factory into commands"
```

## LSW-003 — `LibkrunLiveSession` + `impl LiveSession` (feature-gated)

Wrap the existing libkrun free fns in `session.rs`: `reattach`→`attach`,
`kill_session`→`kill`, `opencode_http`→`http`, `workspace_path`→`workspace_path`,
the server-capture file tailer→`event_source` for a `Server` session,
`ingest`-equivalent→`ingest`. PTY verbs libkrun can't do yet — `send` and
`event_source` for a `Pty` session — return `Caps::unsupported` (NO new vsock
transport; that's Phase 4). `caps()` returns `LibkrunBackend::capabilities()`.
All behind `#[cfg(feature = "libkrun")]`, in `session.rs` (where the wrapped fns
live — avoids a new module decl).

```yaml
id: LSW-003
task_type: feature
archetype: backend
depends_on: ["LSW-001"]
footprint:
  modifies:
    - "src/sandbox/libkrun/session.rs::LibkrunLiveSession"   # new struct + impl LiveSession (file exists)
gate: "cargo clippy --all-targets --features libkrun -- -D warnings clean; cargo test --bin pillbox --features libkrun green; unit test: a LibkrunLiveSession reports caps() per the matrix and send()/event_source() return the unsupported error for a Pty session"
assumptions:
  - "struct holds a cloned Session and decodes LibkrunHandle on demand; in-footprint if it needs more"
  - "#[allow(dead_code)] until Phase 2"
```

## LSW-004 — `live_session(resolved, &Session)` factory (the single dispatch point)

The one place that matches `session.backend` and returns the right
`Box<dyn LiveSession>` — what Phase 2's 8 sites will each call instead of their
own `Backend::parse` arm. Errors clearly on an unknown backend label.

```yaml
id: LSW-004
task_type: feature
archetype: backend
depends_on: ["LSW-002", "LSW-003"]
footprint:
  modifies:
    - "src/sandbox/mod.rs::live_session"   # new fn (file exists; distinct symbol from Caps)
gate: "cargo clippy --all-targets -- -D warnings AND --features libkrun clean; cargo test --bin pillbox (+ --features libkrun) green; unit test: live_session(docker session) → caps().pty_drive==true; live_session(libkrun session) → caps().in_sandbox_grading==true"
assumptions:
  - "libkrun arm is #[cfg(feature=libkrun)]; without the feature the factory errors for a libkrun-labelled session (matches today's behavior)"
```

---

**Self-validation:** 4 tasks. Empty-`depends_on`: LSW-001 only → 25% ≤ 50% ✓.
Concurrent pair LSW-002 ‖ LSW-003 touch `docker.rs` vs `libkrun/session.rs` — no
footprint overlap ✓. `mod.rs` touched by LSW-001 (`::Caps`) and LSW-004
(`::live_session`) but they're transitively ordered, not concurrent ✓. All
`modifies` are symbol-level ✓. Every task has a checkable gate ✓. No cycles;
all `depends_on` ids exist ✓.
