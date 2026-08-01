//! Sandbox backends — the surface that actually runs a configured
//! [`AgentSpec`] for `pillbox run`.
//!
//! The run path is split off of `AgentSpec` into a trait so we can pick
//! at run time between the local backends. The deprecated remote backends
//! (ssh/docker/e2b) were removed in the libkrun pivot — "remote" is
//! becoming the managed/Cloudflare tier; until then pillbox is local-only.
//!
//! - [`libkrun::LibkrunBackend`] — a local libkrun microVM (feature-gated
//!   `libkrun`; the default on that build).
//! - [`docker::DockerBackend`] — host Docker daemon (the no-KVM compat
//!   backend; opt in via `PILLBOX_BACKEND=docker`).
// Context: doc://pillbox/adr-003-qemu-parked@0001#qemu-parked

// The ACP spike is intentionally not wired into a production backend yet.
#[allow(dead_code)]
pub(crate) mod acp;
pub(crate) mod appserver;
pub(crate) mod appserver_client;
pub(crate) mod docker;
pub(crate) mod http;
#[cfg(feature = "libkrun")]
pub(crate) mod libkrun;
pub(crate) mod managed;
pub(crate) mod opencode;
#[cfg(feature = "libkrun")]
pub(crate) mod structured;

use std::path::PathBuf;

use anyhow::Result;

use crate::agents::{AgentSpec, RunOpts};
use crate::errors::PillboxError;
use crate::events::transcripts::TailerHandle;
use crate::pillbox::Pillbox;
use crate::sandbox::http::SandboxHttp;
use crate::session::{Backend, Session};

/// What a backend can do — **declared, not chased**. The plane queries this to
/// decide whether a verb is available on the current substrate, instead of
/// discovering a `bail!("docker only")` buried in a command. Default = all
/// false: a backend opts into each capability it actually supports. The matrix
/// is in docs/substrate-plane.md.
//
// Not every bit has a reader on the current verb set; allow(dead_code) covers
// the unread ones.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    /// `session send` into a PTY agent.
    pub(crate) pty_drive: bool,
    /// Live `watch`/`subscribe`/`wait-idle` for a PTY agent.
    pub(crate) live_pty_tail: bool,
    /// `Integration::Server` agents (opencode, codex-serve).
    pub(crate) server_mode: bool,
    /// `sandbox spawn`/`exec`/`agent` — a long-lived exec target.
    pub(crate) long_lived_exec: bool,
    /// `score --in-sandbox` + `--grader-egress` (a one-shot grader VM).
    pub(crate) in_sandbox_grading: bool,
    /// DNS-level egress allow/deny (not just proxy-level).
    pub(crate) real_egress_fence: bool,
    /// `--detach` + `--vault` together (the vault outlives the CLI).
    pub(crate) detached_vault: bool,
    /// Post-hoc `ingest` of a headless capture (no live host tailer).
    pub(crate) post_hoc_ingest: bool,
}

impl Caps {
    /// The one error a [`LiveSession`] method returns when `verb` isn't
    /// supported on this backend. Centralized so every backend rejects an
    /// absent verb with the same named, exit-2 shape — the capability gap is a
    /// single recognizable error across the plane, not a per-impl `bail!` whose
    /// wording (and exit code) drifts between backends.
    pub(crate) fn unsupported(&self, verb: &str) -> anyhow::Error {
        PillboxError::usage(
            "session",
            format!("`{verb}` isn't supported on this backend"),
        )
        .with_next("pillbox session info <id>  # check the session's backend")
        .into()
    }
}

/// One backend = one way to provision a sandbox + vault session, inject
/// credentials, run the agent, and supervise it. `run()` launches a turn;
/// `capabilities()` reports the backend's verb support. The per-session control
/// surface the command layer drives is the [`LiveSession`] from [`live_session`]
/// (built from a resolved [`Session`]), not the backend directly — see
/// docs/substrate-plane.md. Takes a resolved [`Pillbox`] so the backend can
/// locate the auth home + vault state for the right scope.
pub(crate) trait SandboxBackend {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()>;

    /// What this backend can do — queried before (or instead of) attempting a
    /// verb. The honest current-state profile, per docs/substrate-plane.md.
    fn capabilities(&self) -> Caps;

    /// Stable backend identity (`BACKEND_DOCKER` / `BACKEND_LIBKRUN`) — the
    /// explicit discriminant for code that must branch on *which* backend is
    /// active, rather than inferring it from a capability bit (which silently
    /// breaks the moment two backends share that capability).
    fn id(&self) -> &'static str;
}

/// The live session — **the plane**. A running (foreground or detached) agent
/// the command layer drives and reads *without branching on the backend*: every
/// `session send`/`attach`/`watch`/`kill`/… resolves to one of these. Each
/// method's availability is gated by [`Caps`] — an unsupported verb returns a
/// clear error rather than being a missing match arm. Object-safe, so it rides
/// `Box<dyn LiveSession>`.
///
/// The method set maps 1:1 to the backend-string dispatch sites it replaces
/// (`send`/`attach`/`kill`/`event_source`/`http`/`ingest`).
// Context: doc://pillbox/substrate-plane-livesession@0001#substrate-plane-livesession
pub(crate) trait LiveSession {
    /// This session's backend capabilities (the per-session view of [`Caps`]).
    fn caps(&self) -> Caps;

    /// Drive: push bytes to the agent (PTY input / server prompt). `caps().pty_drive`.
    fn send(&self, bytes: &[u8]) -> Result<()>;

    /// Reattach a terminal to this session.
    fn attach(&self, resolved: &Pillbox) -> Result<()>;

    /// Spawn the tailer that fills this live session's §0 log, returning its guard
    /// (held by the caller for the stream's lifetime; `None` when a producer already
    /// tails). The caller opens the read *source* itself (the local/managed
    /// placement swap), so this returns only the tailer — backend-blind replacement
    /// for the per-backend tailer-spawning in `resolve_streaming_session`.
    fn spawn_log_tailer(&self, resolved: &Pillbox) -> Result<Option<TailerHandle>>;

    /// HTTP handle to a server-mode agent's in-sandbox server. `caps().server_mode`.
    fn http(&self) -> Result<Box<dyn SandboxHttp>>;

    /// Host path of this session's (result) workspace.
    fn workspace_path(&self) -> Result<PathBuf>;

    /// Drain a headless capture into the durable log post-hoc. `caps().post_hoc_ingest`.
    fn ingest(&self, resolved: &Pillbox) -> Result<usize>;

    /// Tear down the backend (kill sandbox/VM) and release its artifacts.
    fn kill(&self, resolved: &Pillbox) -> Result<()>;
}

/// Pick the local backend for one `pillbox run`. The deprecated remote backends
/// (ssh/docker/e2b) were removed in the libkrun pivot — "remote" is becoming the
/// managed/Cloudflare tier; until then pillbox is local-only. On a `libkrun`
/// build the microVM is the default local substrate; Docker is the no-KVM compat
/// opt-out via `PILLBOX_BACKEND=docker`. A build without the feature is always
/// Docker (libkrun isn't compiled in).
pub(crate) fn select_backend() -> Box<dyn SandboxBackend> {
    // `PILLBOX_BACKEND=managed` opts into the managed Cloudflare tier (the §0
    // gateway DO + a CF container). It needs no host capability, so it's
    // selectable on any build — but it requires the managed config env
    // (`PILLBOX_MANAGED_DO_URL`, …); the backend itself reports the gap if it's
    // selected without it. Checked before the local-backend split so it wins on
    // every build.
    if std::env::var_os("PILLBOX_BACKEND").is_some_and(|v| v == "managed") {
        return Box::new(managed::ManagedBackend);
    }
    #[cfg(feature = "libkrun")]
    if std::env::var_os("PILLBOX_BACKEND").is_none_or(|v| v != "docker") {
        return Box::new(libkrun::LibkrunBackend);
    }
    Box::new(docker::DockerBackend)
}

/// Construct the [`LiveSession`] for an existing resolved [`Session`] — the one
/// place that branches on `session.backend`, so the command-layer control verbs
/// (`send`/`attach`/`watch`/`kill`/…) dispatch through the plane instead of each
/// re-matching `Backend::parse`. Construction is just the cloned record; methods
/// that need the resolved [`Pillbox`] take it per-call.
pub(crate) fn live_session(session: &Session) -> Result<Box<dyn LiveSession>> {
    match Backend::parse(&session.backend) {
        Some(Backend::Docker) => Ok(Box::new(docker::DockerLiveSession::new(session.clone()))),
        #[cfg(feature = "libkrun")]
        Some(Backend::Libkrun) => Ok(Box::new(libkrun::LibkrunLiveSession::new(session.clone()))),
        // Managed needs no host capability, so it resolves on every build — the
        // verbs are DO calls, not local-process ops.
        Some(Backend::Managed) => Ok(Box::new(managed::ManagedLiveSession::new(session.clone()))),
        // Usage (exit 2), not config — matches the per-verb wrappers this replaced,
        // so a caller branching on exit code sees no drift for a libkrun record on a
        // build without the feature.
        #[cfg(not(feature = "libkrun"))]
        Some(Backend::Libkrun) => Err(PillboxError::usage(
            "session",
            format!(
                "session `{}` is libkrun-backed, but this pillbox was built without libkrun support",
                session.id
            ),
        )
        .into()),
        None => Err(PillboxError::config(
            "session",
            format!("unknown session backend `{}`", session.backend),
        )
        .into()),
    }
}

/// Drive one structured turn into a server-mode agent (opencode / codex-serve)
/// over its in-sandbox HTTP transport. The shared body behind every host-side
/// backend's [`LiveSession::send`] when the session is [`Integration::Server`], so
/// the command layer drives a server agent through the same polymorphic `send` as a
/// PTY agent — never re-branching on integration or backend. (The managed backend
/// overrides `send` entirely: its turn goes to the DO's `/input`, not an HTTP
/// server the host can reach.)
pub(crate) fn drive_server_prompt(
    session: &Session,
    http: &dyn SandboxHttp,
    bytes: &[u8],
) -> Result<()> {
    let text = String::from_utf8_lossy(bytes);
    // codex-serve's bridge already holds the thread id, so its turn carries only
    // the text; opencode needs the agent session id + model/temperature.
    if session.agent_id == crate::agents::CODEX_SERVE.id {
        return appserver_client::send_turn(http, &text);
    }
    let server = session.server.as_ref().ok_or_else(|| {
        PillboxError::config(
            "session send",
            format!("session `{}` has no server state", session.id),
        )
    })?;
    opencode::send_prompt(
        http,
        &server.agent_session_id,
        &text,
        &server.model,
        server.temperature,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::BACKEND_DOCKER;

    /// Removes `PILLBOX_BACKEND` on drop so the selection test can't leak its
    /// override into another test (the env is process-global). Tests touching
    /// it share the lock below.
    struct BackendEnvGuard;
    impl Drop for BackendEnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("PILLBOX_BACKEND");
        }
    }

    /// Serializes the env-mutating selection test against any other test that
    /// reads/writes `PILLBOX_BACKEND` in this process.
    static BACKEND_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `select_backend` reports a stable id for the chosen backend, so the test
    /// can assert which one was picked without naming a concrete type.
    fn selected_backend_id() -> &'static str {
        if select_backend().capabilities().long_lived_exec {
            BACKEND_DOCKER // docker is the long-lived-exec family
        } else {
            "libkrun"
        }
    }

    #[test]
    fn select_backend_default_and_docker_opt_out() {
        let _lock = BACKEND_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _guard = BackendEnvGuard;

        std::env::remove_var("PILLBOX_BACKEND");
        // Default build: always docker (libkrun isn't compiled in). libkrun
        // build: the microVM is the default substrate.
        #[cfg(not(feature = "libkrun"))]
        assert_eq!(selected_backend_id(), BACKEND_DOCKER);
        #[cfg(feature = "libkrun")]
        assert_eq!(selected_backend_id(), "libkrun");

        // `PILLBOX_BACKEND=docker` is the compat opt-out — docker on every build.
        std::env::set_var("PILLBOX_BACKEND", "docker");
        assert_eq!(selected_backend_id(), BACKEND_DOCKER);

        // An unrecognized value isn't the docker opt-out: it falls through to
        // the build's default (libkrun on a libkrun build, docker otherwise).
        std::env::set_var("PILLBOX_BACKEND", "libkrun");
        #[cfg(not(feature = "libkrun"))]
        assert_eq!(selected_backend_id(), BACKEND_DOCKER);
        #[cfg(feature = "libkrun")]
        assert_eq!(selected_backend_id(), "libkrun");
    }

    #[test]
    fn select_backend_picks_managed_on_opt_in() {
        use crate::session::BACKEND_MANAGED;
        let _lock = BACKEND_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _guard = BackendEnvGuard;
        // `PILLBOX_BACKEND=managed` selects the managed tier on every build (it
        // needs no host capability). Asserted via the stable `id()` discriminant,
        // since managed shares `long_lived_exec=false` with libkrun.
        std::env::set_var("PILLBOX_BACKEND", "managed");
        assert_eq!(select_backend().id(), BACKEND_MANAGED);
    }

    #[test]
    fn live_session_dispatches_managed_to_the_do_plane() {
        use crate::session::{Placement, BACKEND_MANAGED};
        let mut s = Session::test_fixture();
        s.backend = BACKEND_MANAGED.to_string();
        s.placement = Placement::Managed;
        let live = live_session(&s).expect("managed session resolves on every build");
        // Managed is server-mode (structured agent drive + DO read), with no host
        // PTY — the honest profile the plane gates verbs on.
        assert!(live.caps().server_mode);
        assert!(!live.caps().pty_drive);
    }

    #[test]
    fn unsupported_names_the_verb() {
        let err = Caps::default().unsupported("event_source");
        assert!(
            err.to_string().contains("event_source"),
            "error should name the rejected verb, got: {err}"
        );
    }

    #[test]
    fn live_session_dispatches_docker_to_pty_family() {
        let mut s = Session::test_fixture();
        s.backend = BACKEND_DOCKER.to_string();
        let live = live_session(&s).expect("docker session resolves to a LiveSession");
        // Docker is the full-PTY family — the factory must hand back the docker
        // impl, identified here by its capability profile (not a type assert,
        // since the return is a trait object).
        assert!(
            live.caps().pty_drive,
            "docker LiveSession must report pty_drive == true"
        );
    }

    #[cfg(feature = "libkrun")]
    #[test]
    fn live_session_dispatches_libkrun_to_microvm_family() {
        use crate::session::BACKEND_LIBKRUN;
        let mut s = Session::test_fixture();
        s.backend = BACKEND_LIBKRUN.to_string();
        let live = live_session(&s).expect("libkrun session resolves to a LiveSession");
        assert!(
            live.caps().in_sandbox_grading,
            "libkrun LiveSession must report in_sandbox_grading == true"
        );
    }
}
