//! Sandbox backends — the surface that actually runs a configured
//! [`AgentSpec`] for `pillbox run`.
//!
//! A new run is placed on the substrate [`SubstrateKind::selected`] picks, and
//! [`SubstrateKind::backend`] is the one place a substrate becomes an
//! implementation:
//!
//! - [`libkrun::LibkrunBackend`] — a local libkrun microVM (feature-gated
//!   `libkrun`; the default).
//! - [`managed::ManagedBackend`] — the managed Cloudflare execution runtime
//!   (`PILLBOX_BACKEND=managed`).
//!
//! A record's live session or sandbox target is built from the [`Substrate`] it
//! stores ([`live_session`], [`sandbox_target`]).
// QEMU is parked; keep the backend trait seam open for it.

pub(crate) mod appserver;
pub(crate) mod appserver_client;
pub(crate) mod exec_channel;
pub(crate) mod http;
// The build boundary: only code that names this module (or smoltcp) is gated on the libkrun feature.
#[cfg(feature = "libkrun")]
pub(crate) mod libkrun;
pub(crate) mod managed;
pub(crate) mod oci;
pub(crate) mod opencode;
pub(crate) mod structured;
pub(crate) mod substrate;

pub(crate) use substrate::{Substrate, SubstrateKind};

use std::path::PathBuf;

use anyhow::Result;

use crate::agents::{AgentSpec, RunOpts};
use crate::errors::PillboxError;
use crate::events::transcripts::TailerHandle;
use crate::pillbox::Pillbox;
use crate::sandbox::http::SandboxHttp;
use crate::session::Session;

/// A child that outlives its launch call: a detached VMM or §0 tailer. The
/// launch may run inside a long-lived caller (dispatch, eval), where an exited
/// child nobody waits on stays a zombie of that caller. Dropping it hands the
/// child to a waiter thread; the explicit methods consume it instead.
// A detached child is reaped when dropped, so a dead child never reads as alive to signal-0 probes.
pub(crate) struct DetachedChild(Option<std::process::Child>);

impl DetachedChild {
    pub(crate) fn spawn(cmd: &mut std::process::Command) -> std::io::Result<Self> {
        Ok(Self(Some(cmd.spawn()?)))
    }

    /// Take over reaping a child spawned elsewhere.
    pub(crate) fn adopt(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    fn child(&mut self) -> &mut std::process::Child {
        self.0
            .as_mut()
            .expect("a DetachedChild holds its child until consumed")
    }

    pub(crate) fn id(&self) -> u32 {
        self.0.as_ref().map_or(0, std::process::Child::id)
    }

    /// The child's exit status if it has exited (and is then reaped), without blocking.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child().try_wait()
    }

    /// SIGKILL the child (a no-op if it already exited) and wait for it here.
    pub(crate) fn kill_and_wait(mut self) -> std::io::Result<std::process::ExitStatus> {
        let mut child = self.0.take().expect("a DetachedChild holds its child");
        let _ = child.kill();
        child.wait()
    }

    /// Give up the reaper: the caller now owns waiting for the child.
    pub(crate) fn into_child(mut self) -> std::process::Child {
        self.0.take().expect("a DetachedChild holds its child")
    }
}

impl Drop for DetachedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        // Already exited and reaped by a `try_wait`: nothing left to wait for.
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        #[cfg(test)]
        tests::REAPERS_STARTED.with(|n| n.set(n.get() + 1));
        let reaper = std::thread::Builder::new()
            .name(format!("reap-{}", child.id()))
            .spawn(move || {
                let _ = child.wait();
            });
        if let Err(error) = reaper {
            eprintln!(
                "pillbox: warning: could not start a reaper thread for a detached child: {error}"
            );
        }
    }
}

/// What a backend can do — **declared, not chased**. The plane queries this to
/// decide whether a verb is available on the current substrate, instead of
/// discovering a `bail!("libkrun only")` buried in a command. Default = all
/// false: a backend opts into each capability it actually supports. The matrix
/// is in docs/substrate-plane.md.
//
// Not every bit has a reader on the current verb set; allow(dead_code) covers
// the unread ones.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    /// Sealed repository execution with exact file capabilities.
    pub(crate) repository_execution: bool,
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
    fn execute_repository(
        &self,
        _resolved: &Pillbox,
        _repository: &std::path::Path,
        _request: &crate::execution::ExecuteRequest,
        _owner: &mut crate::execution::store::OwnedInvocation,
    ) -> Result<crate::execution::Completion> {
        Err(self.capabilities().unsupported("execution execute"))
    }

    /// Run the sealed project verifier `probe` against the committed tree in
    /// `repository` (`execution verify-project`), printing its JSON record.
    fn verify_project(
        &self,
        _probe: &std::path::Path,
        _repository: &std::path::Path,
    ) -> Result<()> {
        Err(self.capabilities().unsupported("execution verify-project"))
    }

    /// Launch one run and hand back what started (or finished). Backends never
    /// print the outcome: the CLI renders it, and dispatch/eval read it typed.
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<Started>;

    /// What this backend can do — queried before (or instead of) attempting a
    /// verb. The honest current-state profile, per docs/substrate-plane.md.
    fn capabilities(&self) -> Caps;

    /// Which substrate this backend runs — the explicit discriminant for code that
    /// must branch on *which* backend is active, rather than inferring it from a
    /// capability bit (which silently breaks the moment two backends share one).
    fn kind(&self) -> SubstrateKind;

    /// The [`LiveSession`] for a record this backend's substrate runs. Called only
    /// by [`live_session`], with a session whose substrate is [`Self::kind`].
    fn live(&self, session: &Session) -> Box<dyn LiveSession>;

    /// The [`SandboxTarget`] for a sandbox record this backend's substrate runs.
    /// Called only by [`sandbox_target`]. A backend that never spawns sandboxes
    /// can only drop such a record.
    fn target(&self, record: &crate::sandboxes::Sandbox) -> Result<Box<dyn SandboxTarget>> {
        Ok(Box::new(RecordOnly {
            record: record.clone(),
            why: format!(
                "is a {} record, which runs no sandboxes",
                self.kind().label()
            ),
            left: "nothing it ran was torn down".into(),
        }))
    }

    /// Boot a long-lived exec target (`sandbox spawn`) and return its record,
    /// already written to the registry. The caller then addresses it through
    /// [`sandbox_target`]. Only a backend declaring `long_lived_exec` overrides
    /// this; every other backend refuses with the plane's unsupported error.
    fn spawn_sandbox(
        &self,
        _resolved: &Pillbox,
        _req: &SandboxSpawn,
    ) -> Result<crate::sandboxes::Sandbox> {
        Err(self.capabilities().unsupported("sandbox spawn"))
    }

    /// Run `spec`'s interactive login in a throwaway sandbox whose auth home is
    /// `home`, returning the login command's exit code.
    fn login(
        &self,
        _spec: &AgentSpec,
        _resolved: &Pillbox,
        _home: &std::path::Path,
    ) -> Result<i32> {
        Err(self.capabilities().unsupported("auth login"))
    }

    /// Run grader `cmd` over `workspace` in a one-shot sandbox (`session score
    /// --in-sandbox`), returning its exit code and raw output. Only a backend
    /// declaring `in_sandbox_grading` overrides this.
    fn score_in_sandbox(
        &self,
        _resolved: &Pillbox,
        _workspace: &std::path::Path,
        _cmd: &str,
        _egress_allow: &[String],
        _drop_privileges: bool,
    ) -> Result<(i32, String)> {
        Err(self
            .capabilities()
            .unsupported("session score --in-sandbox"))
    }

    /// The host checks `pillbox doctor` adds when this substrate is selected.
    fn host_checks(&self) -> Vec<crate::doctor::Check> {
        Vec::new()
    }
}

/// What `sandbox spawn` asks a backend for: an optional agent harness to
/// provision (its credentials env-forked, the guest user non-root), the host
/// workspace to clone into the sandbox, and an optional `sandbox list` label.
#[derive(Clone, Debug)]
pub(crate) struct SandboxSpawn {
    pub(crate) agent: Option<String>,
    pub(crate) workspace: PathBuf,
    pub(crate) label: Option<String>,
}

/// One live sandbox, addressed by its registry record (see [`sandbox_target`]) —
/// what `sandbox exec`/`agent`/`destroy` drive without branching on the backend.
pub(crate) trait SandboxTarget {
    /// Open a channel for one command. Every refusal (a record pillbox can't
    /// drive, a missing guest workspace, a VM that doesn't answer) happens here,
    /// before anything is sent or reported as started.
    fn open_exec(&self) -> Result<Box<dyn ExecChannel>>;

    /// Run `argv` to completion in the sandbox's workspace, handing each output
    /// chunk to `on_chunk(is_stderr, bytes)` as it arrives (stdin is empty).
    /// Returns the command's exit code (128+signo when a signal killed it).
    fn exec(
        &self,
        argv: &[String],
        on_chunk: &mut dyn FnMut(bool, &[u8]) -> Result<()>,
    ) -> Result<i32> {
        self.open_exec()?.run(argv, on_chunk)
    }

    /// Stop the sandbox, release its artifacts, and delete its record.
    fn destroy(&self, resolved: &Pillbox) -> Result<Destroyed>;
}

/// What [`SandboxTarget::destroy`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Destroyed {
    /// The sandbox was stopped and its files removed, then its record.
    TornDown,
    /// Only the record was dropped: nothing it ran was torn down.
    RecordDropped,
}

/// One opened exec channel ([`SandboxTarget::open_exec`]), good for one command.
pub(crate) trait ExecChannel {
    /// Run `argv` as [`SandboxTarget::exec`] describes.
    fn run(
        self: Box<Self>,
        argv: &[String],
        on_chunk: &mut dyn FnMut(bool, &[u8]) -> Result<()>,
    ) -> Result<i32>;
}

/// Build the [`SandboxTarget`] for a sandbox record from its stored substrate. A
/// retired record resolves to a [`RecordOnly`] target: exec is refused, destroy
/// drops the record and says what it left behind.
// A record's substrate is turned into a backend only through SubstrateKind::backend.
pub(crate) fn sandbox_target(record: &crate::sandboxes::Sandbox) -> Result<Box<dyn SandboxTarget>> {
    match record.substrate.kind() {
        Some(kind) => kind.backend()?.target(record),
        None => Ok(Box::new(RecordOnly::retired(record))),
    }
}

/// A sandbox record pillbox can't drive (see [`sandbox_target`]). Exec is refused;
/// destroy drops the record, says on stderr what it left behind, and succeeds.
struct RecordOnly {
    record: crate::sandboxes::Sandbox,
    /// Why it can't be driven, completing "sandbox `id` …".
    why: String,
    /// What destroy leaves behind, and the manual cleanup if any.
    left: String,
}

impl RecordOnly {
    fn retired(record: &crate::sandboxes::Sandbox) -> Self {
        let why = match &record.substrate {
            Substrate::Retired { backend, why, .. } => {
                format!("is a retired `{backend}` record ({why})")
            }
            _ => String::new(),
        };
        Self {
            record: record.clone(),
            why,
            left: record.substrate.left_behind(),
        }
    }
}

impl SandboxTarget for RecordOnly {
    fn open_exec(&self) -> Result<Box<dyn ExecChannel>> {
        Err(PillboxError::usage(
            "sandbox exec",
            format!(
                "sandbox `{}` {}, so it can't be exec'd (destroy only drops the record)",
                self.record.id, self.why
            ),
        )
        .with_next(format!("pillbox sandbox destroy {}", self.record.id))
        .into())
    }

    fn destroy(&self, resolved: &Pillbox) -> Result<Destroyed> {
        crate::sandboxes::delete(resolved, &self.record.id)?;
        eprintln!(
            "pillbox: sandbox `{}` {}: dropped its record; {}",
            self.record.id, self.why, self.left
        );
        Ok(Destroyed::RecordDropped)
    }
}

/// What one [`SandboxBackend::run`] produced. A backend returns it and never
/// prints it; `commands::run::render_started` is the CLI's rendering, and
/// in-process callers (dispatch, eval) read the session from it directly.
#[derive(Debug)]
pub(crate) enum Started {
    /// A foreground PTY run. The terminal was the whole report; there is no
    /// persisted session to hand back.
    Foreground,
    /// A detached PTY session, left running in the background.
    Detached(Session),
    /// A server-mode agent, up and ready for `session send`. `run` does not
    /// auto-send for server agents, so `pending_prompt` is the prompt `run` was
    /// given (if any), for the caller to send.
    // Only the libkrun backend runs server and structured agents.
    ServerReady {
        session: Session,
        pending_prompt: Option<String>,
    },
    /// A one-shot structured run that has finished. `events` counts the durable
    /// events it wrote, including the terminal one; a non-zero `exit_code` is the
    /// agent's failure, which the caller decides how to surface.
    // Only the libkrun backend runs server and structured agents.
    StructuredFinished {
        session: Session,
        events: usize,
        exit_code: i32,
    },
    /// A managed turn that has finished, with its result snapshot recorded.
    ManagedFinished(Session),
}

impl Started {
    /// The session this run persisted; `None` for a foreground run.
    pub(crate) fn session(&self) -> Option<&Session> {
        match self {
            Started::Foreground => None,
            Started::Detached(session)
            | Started::ServerReady { session, .. }
            | Started::StructuredFinished { session, .. }
            | Started::ManagedFinished(session) => Some(session),
        }
    }

    /// The session a caller can go on driving locally (`session send`,
    /// `wait-idle`, score its workspace): a detached PTY session, a ready
    /// server, or a structured run whose agent exited 0.
    // A failed structured run, a foreground run and a managed run are not drivable local sessions.
    pub(crate) fn into_live_session(self) -> Result<Session> {
        match self {
            Started::Detached(session) | Started::ServerReady { session, .. } => Ok(session),
            Started::StructuredFinished {
                session, exit_code, ..
            } => {
                if exit_code != 0 {
                    anyhow::bail!(
                        "{} structured run `{}` exited {exit_code}",
                        session.agent_id,
                        session.id
                    );
                }
                Ok(session)
            }
            Started::Foreground => {
                anyhow::bail!("started a foreground run, not a detached session")
            }
            Started::ManagedFinished(session) => anyhow::bail!(
                "started a managed run (`{}`), not a local detached session",
                session.id
            ),
        }
    }
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
// Backends dispatch through one LiveSession + Caps, never by matching on the backend name.
pub(crate) trait LiveSession {
    /// This session's backend capabilities (the per-session view of [`Caps`]).
    fn caps(&self) -> Caps;

    /// Drive: push bytes to the agent (PTY input / server prompt). `caps().pty_drive`.
    fn send(&self, resolved: &Pillbox, bytes: &[u8]) -> Result<()>;

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
    // Rule: kill never prints to stdout (in-process callers keep `--json` pure); callers report.
    fn kill(&self, resolved: &Pillbox) -> Result<()>;
}

/// Construct the [`LiveSession`] for an existing resolved [`Session`] from its
/// stored substrate, so the command-layer control verbs (`send`/`attach`/`watch`/
/// `kill`/…) dispatch through the plane. A retired record yields a
/// [`RetiredSession`]: every verb is refused except `kill`, which drops the record.
// A record's substrate is turned into a backend only through SubstrateKind::backend.
pub(crate) fn live_session(session: &Session) -> Result<Box<dyn LiveSession>> {
    match session.substrate.kind() {
        Some(kind) => Ok(kind.backend()?.live(session)),
        None => Ok(Box::new(RetiredSession(session.clone()))),
    }
}

/// A session record left by a removed or unknown backend, or with an undecodable
/// handle (see [`Substrate::Retired`]): nothing about it can be driven.
struct RetiredSession(Session);

impl RetiredSession {
    fn refuse(&self, verb: &str) -> anyhow::Error {
        let why = match &self.0.substrate {
            Substrate::Retired { why, .. } => why.as_str(),
            _ => "",
        };
        PillboxError::usage(
            "session",
            format!(
                "`{verb}` isn't supported on session `{}`: it is a retired `{}` record ({why})",
                self.0.id,
                self.0.substrate.label()
            ),
        )
        .with_next(format!(
            "pillbox session rm {}  # drops the record",
            self.0.id
        ))
        .into()
    }
}

impl LiveSession for RetiredSession {
    fn caps(&self) -> Caps {
        Caps::default()
    }

    fn send(&self, _resolved: &Pillbox, _bytes: &[u8]) -> Result<()> {
        Err(self.refuse("send"))
    }

    fn attach(&self, _resolved: &Pillbox) -> Result<()> {
        Err(self.refuse("attach"))
    }

    fn spawn_log_tailer(&self, _resolved: &Pillbox) -> Result<Option<TailerHandle>> {
        Err(self.refuse("watch"))
    }

    fn http(&self) -> Result<Box<dyn SandboxHttp>> {
        Err(self.refuse("http"))
    }

    fn workspace_path(&self) -> Result<PathBuf> {
        Err(self.refuse("workspace"))
    }

    fn ingest(&self, _resolved: &Pillbox) -> Result<usize> {
        Err(self.refuse("ingest"))
    }

    // pillbox never runs anything for a retired record: kill only drops the record and says what is left.
    fn kill(&self, resolved: &Pillbox) -> Result<()> {
        crate::events::emit_session_event(
            resolved,
            crate::events::EventType::SessionDropped,
            &self.0.id,
            Some(&self.0),
        );
        crate::session::delete(resolved, &self.0.id)?;
        eprintln!(
            "pillbox: session `{}` is a retired `{}` record: dropped its record; {}",
            self.0.id,
            self.0.substrate.label(),
            self.0.substrate.left_behind()
        );
        Ok(())
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
    // the text; opencode needs its agent session id (the model and any
    // temperature were bound when the server and session came up).
    if session.agent_id == crate::agents::CODEX_SERVE.id {
        return appserver_client::send_turn(http, &text);
    }
    let server = session.server.as_ref().ok_or_else(|| {
        PillboxError::config(
            "session send",
            format!("session `{}` has no server state", session.id),
        )
    })?;
    opencode::send_prompt(http, &server.agent_session_id, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_live_or_cleanly_finished_run_is_a_live_session() {
        let s = Session::test_fixture();
        assert_eq!(
            Started::Detached(s.clone()).into_live_session().unwrap().id,
            s.id
        );
        let server = Started::ServerReady {
            session: s.clone(),
            pending_prompt: None,
        };
        assert_eq!(server.session().map(|x| x.id.as_str()), Some(s.id.as_str()));
        assert_eq!(server.into_live_session().unwrap().id, s.id);
        let finished = |exit_code| Started::StructuredFinished {
            session: s.clone(),
            events: 3,
            exit_code,
        };
        assert_eq!(finished(0).into_live_session().unwrap().id, s.id);
        let err = format!("{:#}", finished(2).into_live_session().unwrap_err());
        assert!(err.contains("exited 2"), "{err}");
        assert!(Started::Foreground.session().is_none());
        let err = format!("{:#}", Started::Foreground.into_live_session().unwrap_err());
        assert!(err.contains("foreground"), "{err}");
        let managed = Started::ManagedFinished(s.clone());
        assert!(managed.session().is_some());
        let err = format!("{:#}", managed.into_live_session().unwrap_err());
        assert!(err.contains("managed"), "{err}");
    }

    thread_local! {
        /// Reaper threads `DetachedChild::drop` started on this test's thread.
        pub(super) static REAPERS_STARTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn reapers_started() -> usize {
        REAPERS_STARTED.with(std::cell::Cell::get)
    }

    /// Wait (bounded) until `pid` no longer answers signal-0: it exited AND was reaped.
    fn assert_reaped(pid: i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "exited detached child {pid} still answers signal-0 (zombie)"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn dropping_a_detached_child_reaps_it() {
        // One that may already have exited at drop, and one still running that exits later.
        for (program, args) in [("true", &[][..]), ("sleep", &["0.2"][..])] {
            let child =
                DetachedChild::spawn(std::process::Command::new(program).args(args)).unwrap();
            let pid = child.id() as i32;
            drop(child);
            assert_reaped(pid);
        }
        // A running child is handed to exactly one reaper thread.
        let before = reapers_started();
        let child = DetachedChild::spawn(std::process::Command::new("sleep").arg("0.2")).unwrap();
        let pid = child.id() as i32;
        drop(child);
        assert_eq!(reapers_started(), before + 1);
        assert_reaped(pid);
    }

    #[test]
    fn an_explicitly_stopped_detached_child_is_waited_once() {
        let before = reapers_started();
        let child = DetachedChild::spawn(std::process::Command::new("sleep").arg("30")).unwrap();
        let pid = child.id() as i32;
        let status = child.kill_and_wait().unwrap();
        assert!(!status.success());
        // Waited here, synchronously: no zombie, and no reaper thread waits it again.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
        assert_eq!(reapers_started(), before);

        // A child `try_wait` already reaped needs no reaper either.
        let mut child = DetachedChild::spawn(&mut std::process::Command::new("true")).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(child);
        assert_eq!(reapers_started(), before);

        // Handing the child over gives up the reaper; the new owner waits.
        let child = DetachedChild::spawn(&mut std::process::Command::new("true")).unwrap();
        child.into_child().wait().unwrap();
        assert_eq!(reapers_started(), before);
    }

    #[test]
    fn the_selected_substrate_is_the_backend_kind_it_builds() {
        let managed = SubstrateKind::Managed
            .backend()
            .expect("managed builds everywhere");
        assert_eq!(managed.kind(), SubstrateKind::Managed);
    }

    #[test]
    fn live_session_dispatches_managed_to_the_execution_runtime() {
        let mut s = Session::test_fixture();
        s.substrate = Substrate::Managed(managed::ManagedHandle {
            extra: Default::default(),
            endpoint: "https://w.example.workers.dev".into(),
            execution_session_id: s.id.clone(),
        });
        let live = live_session(&s).expect("managed session resolves on every build");
        // Managed is server-mode (structured execution plus evidence retrieval),
        // with no host PTY — the honest profile the plane gates verbs on.
        assert!(live.caps().server_mode);
        assert!(!live.caps().pty_drive);
    }

    fn sandbox_record(substrate: Substrate) -> crate::sandboxes::Sandbox {
        crate::sandboxes::Sandbox {
            id: "abcdef012345".into(),
            substrate,
            image: "pillbox:latest".into(),
            agent: None,
            workspace: "/work/app".into(),
            workspace_clone: None,
            guest_workspace: None,
            label: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            status: "ready".into(),
        }
    }

    /// Records nothing here can drive: docker's, an unknown backend's, a libkrun
    /// record whose "container-abc" ref isn't a handle, and a managed record (the
    /// managed tier runs no sandboxes).
    fn undrivable_records() -> Vec<crate::sandboxes::Sandbox> {
        ["docker", "e2b", "libkrun"]
            .into_iter()
            .map(|backend| sandbox_record(Substrate::from_legacy(backend, "container-abc")))
            .chain([sandbox_record(Substrate::Managed(managed::ManagedHandle {
                extra: Default::default(),
                endpoint: "https://w.example.workers.dev".into(),
                execution_session_id: "abcdef012345".into(),
            }))])
            .collect()
    }

    #[test]
    fn sandbox_target_refuses_exec_on_records_it_cannot_drive() {
        for record in undrivable_records() {
            let target = sandbox_target(&record).expect("resolves, so it can be destroyed");
            let err = target
                .exec(&["true".into()], &mut |_, _| Ok(()))
                .unwrap_err();
            assert!(err.to_string().contains("can't be exec'd"), "{err}");
            assert!(err.downcast_ref::<PillboxError>().is_some());
        }
    }

    #[test]
    fn destroying_a_record_it_cannot_drive_drops_it_and_succeeds() {
        crate::test_util::with_isolated_home("sandbox-record-only-destroy", || {
            let g = crate::pillbox::global();
            for record in undrivable_records() {
                crate::sandboxes::write(&g, &record).unwrap();
                let destroyed = sandbox_target(&record).unwrap().destroy(&g).unwrap();
                assert_eq!(destroyed, Destroyed::RecordDropped);
                assert!(
                    crate::sandboxes::resolve(&g, &record.id).is_err(),
                    "{} record survived destroy",
                    record.substrate.label()
                );
            }
        });
    }

    #[test]
    fn a_retired_session_refuses_every_verb_but_kill_which_drops_only_the_record() {
        crate::test_util::with_isolated_home("retired-session", || {
            let g = crate::pillbox::global();
            let mut s = Session::test_fixture();
            s.substrate = Substrate::from_legacy("docker", "0123456789ab");
            crate::session::write(&g, &s).unwrap();
            let live = live_session(&s).expect("a retired record resolves");
            assert_eq!(live.caps(), Caps::default());
            let err = live.send(&g, b"hi").unwrap_err();
            assert!(err.to_string().contains("retired `docker`"), "{err}");
            assert!(err.downcast_ref::<PillboxError>().is_some());
            assert!(live.attach(&g).is_err());
            assert!(live.workspace_path().is_err());
            assert!(live.ingest(&g).is_err());
            live.kill(&g).unwrap();
            assert!(crate::session::read(&g, &s.id).unwrap().is_none());
        });
    }

    #[test]
    fn spawn_sandbox_defaults_to_unsupported() {
        let req = SandboxSpawn {
            agent: None,
            workspace: PathBuf::from("/work/app"),
            label: None,
        };
        let err = managed::ManagedBackend
            .spawn_sandbox(&crate::pillbox::global(), &req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sandbox spawn"), "got: {err}");
    }

    #[test]
    fn unsupported_names_the_verb() {
        let err = Caps::default().unsupported("event_source");
        assert!(
            err.to_string().contains("event_source"),
            "error should name the rejected verb, got: {err}"
        );
    }

    #[cfg(feature = "libkrun")]
    #[test]
    fn live_session_dispatches_libkrun_to_microvm_family() {
        assert_eq!(
            SubstrateKind::Libkrun.backend().unwrap().kind(),
            SubstrateKind::Libkrun
        );
        let s = Session::test_fixture();
        let live = live_session(&s).expect("libkrun session resolves to a LiveSession");
        assert!(
            live.caps().in_sandbox_grading,
            "libkrun LiveSession must report in_sandbox_grading == true"
        );
    }
}
