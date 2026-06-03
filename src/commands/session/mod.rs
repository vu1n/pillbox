//! `pillbox session …` handlers — list/info/attach/detach/rm/pull/
//! prune + the `started`/`done` event emitters used by the sandbox-
//! side wrapper. main.rs's `Command::Session { action }` arm calls
//! into [`dispatch`]; everything else here is private to the module.
//!
//! Lives in `commands/` (not the registry-storage `session.rs`) so
//! the CLI surface stays decoupled from the on-disk record format.

use anyhow::Result;

use crate::agents::Integration;
use crate::cli::{DoneStatus, SessionAction};
use crate::docker::DockerEndpoint;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{events, remote, sandbox, session};

mod stream;

/// A [`SandboxHttp`](sandbox::http::SandboxHttp) to a server-mode session's
/// in-sandbox opencode server — `docker exec curl` for a (local/remote) docker
/// session, an HTTP-over-vsock client for a libkrun one. The dispatch axis for
/// `session send`/`subscribe`/`watch` on a `Server`-integration agent.
fn opencode_http(
    resolved: &Pillbox,
    s: &session::Session,
) -> Result<Box<dyn sandbox::http::SandboxHttp>> {
    let port = sandbox::opencode::SERVE_PORT;
    match session::Backend::parse(&s.backend) {
        Some(session::Backend::Docker) => Ok(Box::new(sandbox::http::DockerHttp::new(
            DockerEndpoint::local(),
            s.sandbox_id.clone(),
            port,
        ))),
        Some(session::Backend::RemoteDocker) => {
            let remote = remote::resolve_run_target(resolved, &s.remote)?;
            let endpoint = sandbox::remote_docker::endpoint_for(&remote)?;
            Ok(Box::new(sandbox::http::DockerHttp::new(
                endpoint,
                s.sandbox_id.clone(),
                port,
            )))
        }
        Some(session::Backend::Libkrun) => libkrun_opencode_http(s),
        _ => Err(PillboxError::usage(
            "session",
            format!(
                "opencode server sessions need a docker or libkrun backend (got `{}`)",
                s.backend
            ),
        )
        .into()),
    }
}

/// libkrun opencode HTTP transport (feature-gated; see [`opencode_http`]).
#[cfg(feature = "libkrun")]
fn libkrun_opencode_http(
    s: &session::Session,
) -> Result<Box<dyn sandbox::http::SandboxHttp>> {
    sandbox::libkrun::opencode_http(s)
}
#[cfg(not(feature = "libkrun"))]
fn libkrun_opencode_http(
    _s: &session::Session,
) -> Result<Box<dyn sandbox::http::SandboxHttp>> {
    Err(PillboxError::usage("session", "this libkrun session needs the libkrun feature built").into())
}

/// §0 read for a libkrun server session: drain its persistent `/event` capture
/// file (replay everything + follow appends via [`FollowReader`]) into the log.
/// The gateway-free, complete-capture source — unlike the live bridge, a late
/// watcher still gets the whole history because the file persisted.
#[cfg(feature = "libkrun")]
fn libkrun_opencode_file_tailer(
    s: &session::Session,
    log: crate::events::log::SessionLog,
) -> Option<events::transcripts::TailerHandle> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let path = sandbox::libkrun::opencode_events_file(s)
        .map_err(|e| eprintln!("pillbox: note: can't locate the opencode events file ({e})"))
        .ok()?;
    // FollowReader opens the path lazily — it waits for the guest to create the
    // file (first SSE line), so a `watch` right after `run` (before any events)
    // doesn't miss it. Terminating is the shared `stop` flag's job.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let sid = s.id.clone();
    let join = std::thread::spawn(move || {
        let mut log = log;
        let reader = crate::events::opencode::FollowReader::new(path, Arc::clone(&stop_thread));
        if let Err(e) = crate::events::opencode::drain_sse(reader, &sid, &mut log, &stop_thread) {
            eprintln!("pillbox: warning: opencode events drain stopped: {e:#}");
        }
    });
    // The shared `stop` flag terminates the FollowReader (→ the drain ends), so
    // there's nothing to tear down beyond flipping it.
    Some(events::transcripts::TailerHandle::from_flag(stop, join))
}
#[cfg(not(feature = "libkrun"))]
fn libkrun_opencode_file_tailer(
    _s: &session::Session,
    _log: crate::events::log::SessionLog,
) -> Option<events::transcripts::TailerHandle> {
    None
}

/// Run a grader in a one-shot microVM (`session score --in-sandbox`); feature-gated.
#[cfg(feature = "libkrun")]
fn libkrun_score_in_sandbox(
    resolved: &Pillbox,
    workspace: &std::path::Path,
    cmd: &str,
    egress_allow: &[String],
) -> Result<(i32, String)> {
    sandbox::libkrun::score_in_sandbox(resolved, workspace, cmd, egress_allow)
}
#[cfg(not(feature = "libkrun"))]
fn libkrun_score_in_sandbox(
    _resolved: &Pillbox,
    _workspace: &std::path::Path,
    _cmd: &str,
    _egress_allow: &[String],
) -> Result<(i32, String)> {
    Err(PillboxError::usage(
        "session score",
        "--in-sandbox requires the libkrun feature (host grading: drop --in-sandbox)",
    )
    .into())
}

/// Drain a libkrun opencode session's persisted `/event` capture file into its
/// durable log (feature-gated). Plain `File` → `drain_sse` reads to EOF (it
/// final-flushes a trailing partial frame) and returns; the never-set `stop`
/// flag is only meaningful on the follow path. Returns the §0 event count.
#[cfg(feature = "libkrun")]
fn libkrun_ingest_events_file(resolved: &Pillbox, s: &session::Session) -> Result<usize> {
    use std::sync::atomic::AtomicBool;
    let path = sandbox::libkrun::opencode_events_file(s)?;
    let file = std::fs::File::open(&path).map_err(|e| {
        PillboxError::runtime("session ingest", format!("open events file {}: {e}", path.display()))
            .with_next("the session may not have produced any §0 events yet")
    })?;
    let mut log = crate::events::log::SessionLog::open(resolved, &s.id)?;
    let stop = AtomicBool::new(false);
    crate::events::opencode::drain_sse(file, &s.id, &mut log, &stop)
}
#[cfg(not(feature = "libkrun"))]
fn libkrun_ingest_events_file(_resolved: &Pillbox, _s: &session::Session) -> Result<usize> {
    Err(PillboxError::usage("session ingest", "this libkrun session needs the libkrun feature built").into())
}

/// Host path of a libkrun session's result-workspace (the agent's CoW clone),
/// or None for other backends / non-libkrun builds. Feature-gated.
#[cfg(feature = "libkrun")]
fn libkrun_workspace_path(s: &session::Session) -> Option<String> {
    if !matches!(session::Backend::parse(&s.backend), Some(session::Backend::Libkrun)) {
        return None;
    }
    sandbox::libkrun::workspace_path(s)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}
#[cfg(not(feature = "libkrun"))]
fn libkrun_workspace_path(_s: &session::Session) -> Option<String> {
    None
}

pub(crate) fn dispatch(resolved: &Pillbox, action: SessionAction) -> Result<()> {
    match action {
        SessionAction::List { json } => session_list(resolved, json),
        SessionAction::Info { id, json } => session_info(resolved, &id, json),
        SessionAction::Diagnose { id, json } => session_diagnose(resolved, &id, json),
        SessionAction::Attach { id } => session_attach(resolved, &id),
        SessionAction::Detach { id } => session_detach(resolved, &id),
        SessionAction::Rm { id } => session_rm(resolved, &id),
        SessionAction::Send { id, text } => session_send(resolved, &id, &text),
        SessionAction::Subscribe { id, from, bind } => {
            stream::session_subscribe(resolved, &id, from, bind.as_deref())
        }
        SessionAction::Watch { id, from } => stream::session_watch(resolved, &id, from),
        SessionAction::Events { follow, json } => events::dispatch_events(resolved, follow, json),
        SessionAction::Started { id } => session_started(resolved, &id),
        SessionAction::Done {
            id,
            status,
            reason,
            exit_code,
            trace_path,
            result_snapshot,
        } => session_done(
            resolved,
            &id,
            status,
            reason,
            exit_code,
            trace_path,
            result_snapshot,
        ),
        SessionAction::Pull { id, to } => session_pull(resolved, &id, to.as_deref()),
        SessionAction::Score {
            id,
            cmd,
            snapshot,
            workspace,
            in_sandbox,
            grader_egress,
        } => session_score(
            resolved,
            &id,
            &cmd,
            snapshot.as_deref(),
            workspace.as_deref(),
            in_sandbox,
            &grader_egress,
        ),
        SessionAction::Ingest { id, json } => session_ingest(resolved, &id, json),
        SessionAction::WaitIdle { id, timeout, from } => {
            stream::session_wait_idle(resolved, &id, timeout, from)
        }
        SessionAction::Prune { dry_run } => session_prune(resolved, dry_run),
        SessionAction::Transcript {
            file,
            session_id,
            agent,
            follow,
        } => session_transcript(&file, &session_id, agent, follow),
    }
}

fn session_transcript(
    file: &std::path::Path,
    session_id: &str,
    agent: Option<crate::cli::TranscriptAgent>,
    follow: bool,
) -> Result<()> {
    use crate::cli::TranscriptAgent;
    let harness = match agent {
        Some(TranscriptAgent::Claude) => events::transcripts::Harness::Claude,
        Some(TranscriptAgent::Codex) => events::transcripts::Harness::Codex,
        None => events::transcripts::Harness::from_path(file),
    };
    if follow {
        // Manual drain (`session transcript`) synthesizes chat spans too,
        // so Workshop's Overview renders — there's no MITM here to do it.
        let mut tailer = events::transcripts::Tailer::new(
            file.to_path_buf(),
            session_id.into(),
            harness,
            true,
            None, // manual drain to OTLP; not a live run's durable spine
        );
        eprintln!(
            "pillbox: tailing {} → session_id={session_id} (harness={harness:?}); Ctrl-C to stop",
            file.display(),
        );
        let total = tailer.follow()?;
        eprintln!("pillbox: emitted {total} transcript event(s) before exit");
    } else {
        let count = events::transcripts::drain_file_as(file, session_id, harness)?;
        eprintln!(
            "pillbox: drained {count} transcript event(s) from {} → session_id={session_id} \
             (harness={harness:?})",
            file.display(),
        );
    }
    Ok(())
}

/// `Session::to_json_value` with the derived `status` merged on top — `status`
/// isn't a stored field, so this is the single shape `list`/`info`/`diagnose`
/// all emit (and `diagnose` then layers its extra fields).
fn session_json_with_status(
    s: &session::Session,
    status: events::status::SessionStatus,
) -> serde_json::Value {
    let mut v = s.to_json_value();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("status".into(), status.label().into());
    }
    v
}

fn session_list(resolved: &Pillbox, json: bool) -> Result<()> {
    let entries = session::list(resolved)?;
    // One pass over the shared lifecycle sink for every session's terminal
    // outcome, then derive each status from that + its per-session log.
    let terminal = events::status::terminal_outcomes(resolved)?;
    if json {
        // Single source of truth for the on-wire shape lives on
        // `Session::to_json_value` so list + info stay in lockstep; the
        // derived `status` is merged on top (it's not a stored field).
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|s| {
                let status = events::status::derive(resolved, s, terminal.get(&s.id))?;
                Ok(session_json_with_status(s, status))
            })
            .collect::<Result<_>>()?;
        println!(
            "{}",
            crate::paths::json_v1(vec![
                ("pillbox", resolved.display_name().into()),
                ("sessions", serde_json::Value::Array(arr))
            ])
        );
        return Ok(());
    }
    if entries.is_empty() {
        println!("(no sessions in `{}`)", resolved.display_name());
        println!();
        println!("Start one with: pillbox run --remote NAME --detach");
        return Ok(());
    }
    println!(
        "Sessions in `{}` (id        status       attached?  agent    remote          started_at):",
        resolved.display_name()
    );
    for s in &entries {
        let status = events::status::derive(resolved, s, terminal.get(&s.id))?;
        let attached = match s.attached_pid {
            Some(_) => "active  ",
            None => "detached",
        };
        let label = s
            .label
            .as_deref()
            .map(|l| format!(" [{l}]"))
            .unwrap_or_default();
        println!(
            "  {}  {:<11}  {attached}  {:<7}  {:<14}  {}{label}",
            s.id,
            status.label(),
            s.agent_id,
            s.remote,
            s.started_at
        );
    }
    Ok(())
}

fn session_info(resolved: &Pillbox, id: &str, json: bool) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    let status = events::status::derive_one(resolved, &s)?;
    if json {
        let mut v = session_json_with_status(&s, status);
        // Expose the host path of the result-workspace when the backend has one
        // (libkrun: the agent's CoW clone) — so graders/orchestrators read it
        // from this surface instead of parsing the session record.
        if let (Some(obj), Some(ws)) = (v.as_object_mut(), libkrun_workspace_path(&s)) {
            obj.insert("workspace".into(), ws.into());
        }
        println!("{}", crate::paths::json_v1(vec![("session", v)]));
        return Ok(());
    }
    println!("Session: {}", s.id);
    println!("  status:       {}", status.label());
    if let Some(label) = &s.label {
        println!("  label:        {label}");
    }
    println!("  remote:       {}", s.remote);
    println!("  backend:      {}", s.backend);
    println!("  sandbox_id:   {}", s.sandbox_id);
    println!("  pty_pid:      {}", s.pty_pid);
    println!("  agent:        {}", s.agent_id);
    println!("  started_at:   {}", s.started_at);
    println!(
        "  attached_pid: {}",
        match s.attached_pid {
            Some(p) => p.to_string(),
            None => "(detached)".to_string(),
        }
    );
    Ok(())
}

/// `pillbox session diagnose ID` — the "what happened / why is it stuck"
/// companion to `session info` (which just dumps the record). Folds the
/// derived status, the terminal failure detail, and an activity summary from
/// the durable log into one readout.
fn session_diagnose(resolved: &Pillbox, id: &str, json: bool) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    let terminals = events::status::terminal_outcomes(resolved)?;
    let terminal = terminals.get(&s.id);
    // One fold (shared with `list`/`info`): status + activity counts.
    let d = events::status::summarize(resolved, &s, terminal)?;

    let (fail_reason, exit_code) = match terminal {
        Some(events::status::Terminal::Failed { reason, exit_code }) => {
            (Some(reason.as_str()), *exit_code)
        }
        Some(events::status::Terminal::Done { exit_code }) => (None, *exit_code),
        None => (None, None),
    };

    if json {
        let mut v = session_json_with_status(&s, d.status);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("log_seq".into(), d.log_seq.into());
            obj.insert("assistant_turns".into(), d.assistant_turns.into());
            obj.insert("tool_calls".into(), d.tool_calls.into());
            if !d.last_at.is_empty() {
                obj.insert("last_event_at".into(), d.last_at.clone().into());
            }
            if let Some(r) = fail_reason {
                obj.insert("failure_reason".into(), r.into());
            }
            if let Some(c) = exit_code {
                obj.insert("exit_code".into(), c.into());
            }
        }
        println!("{}", crate::paths::json_v1(vec![("session", v)]));
        return Ok(());
    }

    println!("Session {} — {}", s.id, d.status.label());
    if let Some(r) = fail_reason {
        let suffix = exit_code
            .map(|c| format!(" (exit {c})"))
            .unwrap_or_default();
        println!("  failure:      {r}{suffix}");
    } else if let Some(c) = exit_code {
        println!("  exit_code:    {c}");
    }
    if d.status == events::status::SessionStatus::NeedsInput {
        println!(
            "  awaiting:     input — the agent ended its turn; drive it with `pillbox session send {} …`",
            s.id
        );
    }
    println!("  agent:        {}", s.agent_id);
    println!("  remote:       {} ({})", s.remote, s.backend);
    println!("  started_at:   {}", s.started_at);
    println!(
        "  attached:     {}",
        match s.attached_pid {
            Some(p) => format!("yes (pid {p})"),
            None => "no (detached)".to_string(),
        }
    );
    if let Some(e) = &s.expires_at {
        println!("  expires_at:   {e}");
    }
    println!(
        "  result_snap:  {}",
        s.result_snapshot
            .as_deref()
            .unwrap_or("(none — not finished, or no result pushed)")
    );
    println!("Activity (durable log):");
    if d.log_seq == 0 {
        println!("  (no host-visible log — a remote session streams its transcript sandbox-side)");
    } else {
        println!(
            "  {} assistant turn(s), {} tool call(s), {} event(s)",
            d.assistant_turns, d.tool_calls, d.log_seq
        );
        if !d.last_at.is_empty() {
            println!("  last event at {}", d.last_at);
        }
    }
    Ok(())
}

fn session_attach(resolved: &Pillbox, id: &str) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    // Server-integration agents (opencode) have no PTY to attach — they're
    // driven/read over HTTP. Point at the right verbs instead of pumping
    // garbage frames over the HTTP transport.
    if s.integration() == Integration::Server {
        return Err(PillboxError::usage(
            "session attach",
            format!("`{}` is a server-mode session (no PTY)", s.agent_id),
        )
        .with_next(format!(
            "pillbox session watch {id}   # read it    ·   pillbox session send {id} \"…\"   # drive it"
        ))
        .into());
    }
    match session::Backend::parse(&s.backend) {
        // Local Docker attaches to the host daemon directly (no remote);
        // docker:// re-resolves the endpoint from `remote`.
        Some(session::Backend::Docker) => sandbox::local_docker::reattach(resolved, &s),
        Some(session::Backend::RemoteDocker) => {
            let remote = remote::resolve_run_target(resolved, &s.remote)?;
            sandbox::remote_docker::reattach(resolved, &remote, &s)
        }
        Some(session::Backend::E2b) => {
            let remote = remote::read(resolved, &s.remote)?.ok_or_else(|| {
                PillboxError::runtime(
                    "session attach",
                    format!(
                        "remote `{}` is no longer registered — session record is orphaned",
                        s.remote
                    ),
                )
                .with_next(format!("pillbox session rm {}", s.id))
            })?;
            sandbox::remote_e2b::reattach(resolved, &remote, &s)
        }
        Some(session::Backend::Ssh) => {
            let remote = remote::read(resolved, &s.remote)?.ok_or_else(|| {
                PillboxError::runtime(
                    "session attach",
                    format!(
                        "remote `{}` is no longer registered — session record is orphaned",
                        s.remote
                    ),
                )
                .with_next(format!("pillbox session rm {}", s.id))
            })?;
            sandbox::remote_ssh::reattach(resolved, &remote, &s)
        }
        Some(session::Backend::Libkrun) => libkrun_reattach(resolved, &s),
        None => Err(PillboxError::config(
            "session attach",
            format!("unknown session backend `{}`", s.backend),
        )
        .into()),
    }
}

/// Dispatch a libkrun reattach (feature-gated). A `libkrun` session record can
/// exist on disk even in a build without the feature; fail clearly there.
fn libkrun_reattach(_resolved: &Pillbox, _s: &session::Session) -> Result<()> {
    #[cfg(feature = "libkrun")]
    {
        sandbox::libkrun::reattach(_resolved, _s)
    }
    #[cfg(not(feature = "libkrun"))]
    {
        Err(PillboxError::usage(
            "session attach",
            "this is a libkrun session but this build wasn't compiled with the `libkrun` feature",
        )
        .into())
    }
}

fn session_detach(resolved: &Pillbox, id: &str) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    let pid = match s.attached_pid {
        Some(p) => p,
        None => {
            println!("(session `{}` is already detached)", s.id);
            return Ok(());
        }
    };
    // SIGTERM the attached pillbox process. Its attach pump installs a
    // SIGTERM handler (detach_enabled) that resolves the session as
    // `Detached` → the reattach path marks the session detached and prints
    // the reattach hint (then tears down its own transport). We clear the
    // attached_pid field below as a belt-and-suspenders — if the attached
    // pillbox has crashed already, its cleanup never ran.
    //
    // The session record is user-writable TOML — `attached_pid` could
    // be hand-edited (or stale after a pillbox crash + pid reuse).
    // Defenses, in order:
    //   1. Reject pid <= 1 and our own pid up front — kill(0, _)
    //      signals the whole process group; kill(-1, _) broadcasts;
    //      kill(1, _) targets init/launchd. None of those can ever be
    //      a pillbox we spawned.
    //   2. Range-check into libc::pid_t (i32 on Linux/macOS).
    //   3. Probe with kill(pid, 0) first: if the pid no longer exists
    //      we treat the session as already-detached without sending a
    //      signal to a recycled process.
    // We cannot fully defeat pid reuse races (the kernel could recycle
    // between probe and SIGTERM); these checks shrink the window and
    // reject the obviously-wrong cases.
    #[cfg(unix)]
    {
        let self_pid = i64::from(std::process::id());
        if pid <= 1 || pid == self_pid {
            return Err(PillboxError::runtime(
                "session detach",
                format!(
                    "refusing to signal pid {pid} (reserved / self) — session record may be \
                     corrupted; inspect `pillbox session info {}` and clear with `pillbox session rm`",
                    s.id
                ),
            )
            .into());
        }
        let target = libc::pid_t::try_from(pid).map_err(|_| {
            PillboxError::runtime("session detach", format!("pid {pid} out of range"))
        })?;
        // Liveness probe: signal 0 returns 0 iff the pid exists AND we
        // have permission to signal it. If ESRCH the attached pillbox
        // already exited; clear the stamp and return without firing
        // SIGTERM at whatever recycled pid now lives there.
        // SAFETY: signal 0 performs no signal delivery, only the pid
        // and permission checks. Always safe to call.
        let probe = unsafe { libc::kill(target, 0) };
        if probe != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                eprintln!("pillbox: warning: attached pid {pid} no longer exists; clearing stamp.");
                session::mark_detached(resolved, &s.id)?;
                return Ok(());
            }
            // EPERM or other: the pid exists but isn't ours to signal.
            // Refuse rather than try.
            return Err(PillboxError::runtime(
                "session detach",
                format!("kill probe pid {pid}: {err}"),
            )
            .into());
        }
        // SAFETY: SIGTERM to a validated pid we just confirmed is
        // signalable by this uid; no signal handler installed on this
        // side; we own the target process (it's another pillbox we
        // spawned).
        let rc = unsafe { libc::kill(target, libc::SIGTERM) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                eprintln!("pillbox: warning: kill {pid}: {err}");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        return Err(PillboxError::resource(
            "session detach",
            "session detach requires SIGTERM (Unix only) in v0.6",
        )
        .into());
    }
    session::mark_detached(resolved, &s.id)?;
    println!("pillbox: ✓ session `{}` detach signalled.", s.id);
    Ok(())
}

fn session_rm(resolved: &Pillbox, id: &str) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    match session::Backend::parse(&s.backend) {
        Some(session::Backend::Docker) => sandbox::local_docker::kill_session(resolved, &s),
        Some(session::Backend::RemoteDocker) => {
            // `.ok()` (not `?`): a deregistered remote must not strand the
            // local record — kill_session drops it either way.
            let remote = remote::resolve_run_target(resolved, &s.remote).ok();
            sandbox::remote_docker::kill_session(resolved, remote.as_ref(), &s)
        }
        Some(session::Backend::E2b) => sandbox::remote_e2b::kill_session(resolved, &s),
        Some(session::Backend::Ssh) => {
            // ssh teardown needs the registered remote to reach the host, but
            // a missing remote must NOT strand the local record. Pass it
            // through as Option and let kill_session drop the record either
            // way (warning if it couldn't reach the host) — mirrors e2b's
            // "drop the record regardless".
            let remote = remote::read(resolved, &s.remote)?;
            sandbox::remote_ssh::kill_session(resolved, remote.as_ref(), &s)
        }
        Some(session::Backend::Libkrun) => libkrun_kill_session(resolved, &s),
        None => Err(PillboxError::config(
            "session rm",
            format!("unknown session backend `{}`", s.backend),
        )
        .into()),
    }
}

/// Dispatch a libkrun teardown (feature-gated; see [`libkrun_reattach`]). Without
/// the feature we can't kill the VM, but still drop the orphaned record.
fn libkrun_kill_session(resolved: &Pillbox, s: &session::Session) -> Result<()> {
    #[cfg(feature = "libkrun")]
    {
        sandbox::libkrun::kill_session(resolved, s)
    }
    #[cfg(not(feature = "libkrun"))]
    {
        eprintln!("pillbox: warning: libkrun feature not built — can't kill the VM; dropping the record only");
        session::delete(resolved, &s.id)?;
        Ok(())
    }
}

fn session_send(resolved: &Pillbox, id: &str, text: &str) -> Result<()> {
    // Drive targets a RUNNING session (a pty-host to write to), so resolve
    // against the session registry — not `resolve_logged` (the read side's
    // foreground log dirs).
    let s = session::resolve(resolved, id)?;
    // Server-integration agents (opencode) are driven over their HTTP prompt
    // API, not a pty-relay: `session send` = a structured prompt, not keystrokes.
    if s.integration() == Integration::Server {
        let http = opencode_http(resolved, &s)?;
        let server = s.server.as_ref().ok_or_else(|| {
            PillboxError::config(
                "session send",
                format!("session `{}` has no opencode server state", s.id),
            )
        })?;
        sandbox::opencode::send_prompt(&*http, &server.agent_session_id, text, &server.model)?;
        eprintln!("pillbox: sent prompt to opencode session `{}`", s.id);
        return Ok(());
    }
    match session::Backend::parse(&s.backend) {
        Some(session::Backend::Docker) => {
            sandbox::local_docker::send_input(&s.sandbox_id, text.as_bytes())?;
            eprintln!("pillbox: sent {} byte(s) to session `{}`", text.len(), s.id);
            Ok(())
        }
        Some(session::Backend::RemoteDocker) => {
            let remote = remote::resolve_run_target(resolved, &s.remote)?;
            let endpoint = sandbox::remote_docker::endpoint_for(&remote)?;
            sandbox::remote_docker::send_input(&endpoint, &s.sandbox_id, text.as_bytes())?;
            eprintln!("pillbox: sent {} byte(s) to session `{}`", text.len(), s.id);
            Ok(())
        }
        // e2b/ssh nest the agent in a remote-host container; the same relay-exec
        // drive extends there (mirroring their reattach transport), not wired yet.
        Some(_) => Err(PillboxError::usage(
            "session send",
            format!(
                "send isn't supported for `{}` sessions yet (docker only)",
                s.backend
            ),
        )
        .with_next(format!("pillbox session attach {}", s.id))
        .into()),
        None => Err(PillboxError::config(
            "session send",
            format!("unknown session backend `{}`", s.backend),
        )
        .into()),
    }
}

fn session_pull(resolved: &Pillbox, id: &str, to: Option<&std::path::Path>) -> Result<()> {
    use crate::workspace::{SnapshotHandle, WorkspaceBackend};
    let session = session::resolve(resolved, id)?;
    let handle_str = session.result_snapshot.as_ref().ok_or_else(|| {
        PillboxError::runtime(
            "session pull",
            format!(
                "session `{}` has no result_snapshot yet — the agent hasn't \
                 finished (or never called `pillbox session done --result-snapshot`)",
                session.id
            ),
        )
        .with_next(format!("pillbox session info {}", session.id))
    })?;
    let backend = resolved.workspace()?;
    let target = match to {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| PillboxError::runtime("session pull", format!("resolve cwd: {e}")))?
            .join(format!("session-{}", session.id)),
    };
    std::fs::create_dir_all(&target)
        .map_err(|e| PillboxError::runtime("session pull", format!("create {target:?}: {e}")))?;
    let handle = SnapshotHandle::new(handle_str.clone());
    backend.pull(&target, Some(&handle))?;
    println!(
        "pillbox: ✓ restored session `{}` (snapshot `{}`) into {}",
        session.id,
        handle.short(),
        target.display()
    );
    Ok(())
}

/// Drain a session's durable raw §0 capture into its `log.jsonl`, post-hoc and
/// idempotent — the "§0 drain" for headless/batch runs (the optimization loop)
/// where no live `watch`/`subscribe` filled the log. The reparented guest
/// outlives `run`, so a host-side live tailer can't persist for it; but the
/// guest's `/event` capture file does, so we read it to EOF after the turn and
/// the full trajectory lands in the canonical log. Idempotent via a `.ingested`
/// marker: a second call is a no-op (re-draining would duplicate, since the log
/// stamps fresh seqs). Run BEFORE `session score` so trajectory events precede
/// the `scored` event in seq order.
fn session_ingest(resolved: &Pillbox, id: &str, json: bool) -> Result<()> {
    let session = session::resolve(resolved, id)?;
    let marker = session::session_dir(resolved, &session.id)?.join(".ingested");
    if marker.exists() {
        if json {
            println!(
                r#"{{"version":1,"session":"{}","ingested":0,"status":"already-ingested"}}"#,
                session.id
            );
        } else {
            println!("pillbox: session `{}` already ingested (no-op)", session.id);
        }
        return Ok(());
    }

    // Only the file-capture path is supported: libkrun opencode persists `/event`
    // to a guest-home file we can read post-hoc. docker server sessions drain
    // live through the HTTP bridge, and PTY agents through the transcript tailer
    // — both via `session subscribe`/`watch`, which fill the same log.
    let is_libkrun = matches!(
        session::Backend::parse(&session.backend),
        Some(session::Backend::Libkrun)
    );
    if !is_libkrun || session.integration() != Integration::Server {
        return Err(PillboxError::usage(
            "session ingest",
            format!(
                "ingest currently supports libkrun opencode (server) sessions; \
                 `{}`/{:?} drains live via `session subscribe`/`watch`",
                session.backend,
                session.integration()
            ),
        )
        .into());
    }

    let n = libkrun_ingest_events_file(resolved, &session)?;
    std::fs::write(&marker, b"").map_err(|e| {
        PillboxError::runtime("session ingest", format!("write ingest marker: {e}"))
    })?;
    if json {
        println!(
            r#"{{"version":1,"session":"{}","ingested":{n},"status":"ok"}}"#,
            session.id
        );
    } else {
        println!(
            "pillbox: ✓ drained {n} §0 event(s) into session `{}` log",
            session.id
        );
    }
    Ok(())
}

/// Externally grade a session's result and record it as a verifiable `scored`
/// §0 event — the reward channel the optimization loops gate on. Runs `cmd`
/// (via `sh -c`) with cwd = the graded workspace: `workspace` if given, else a
/// rehydrated snapshot (`snapshot`, or the session's `result_snapshot`). The
/// grader's **exit status** is the verifiable pass/fail (NOT the agent's
/// `session done` self-report); its combined output is the feedback gradient.
/// The grader runs on the HOST — it's the invoker's own `--cmd`, like a Makefile
/// target; a sandboxed grader is a future upgrade.
fn session_score(
    resolved: &Pillbox,
    id: &str,
    cmd: &str,
    snapshot: Option<&str>,
    workspace: Option<&std::path::Path>,
    in_sandbox: bool,
    grader_egress: &[String],
) -> Result<()> {
    use crate::workspace::{SnapshotHandle, WorkspaceBackend};

    // Egress is a property of the in-sandbox grader-VM's network fence; on the
    // host grader there's nothing to fence (it already has the host's network).
    if !grader_egress.is_empty() && !in_sandbox {
        return Err(PillboxError::usage(
            "session score",
            "--grader-egress only applies to --in-sandbox (the host grader uses the host network)",
        )
        .into());
    }
    let session = session::resolve(resolved, id)?;

    // Resolve the dir to grade. --workspace wins; else rehydrate a snapshot into
    // a tempdir that must outlive the grader, so keep it bound until fn end.
    let mut _grade_tmp: Option<tempfile::TempDir> = None;
    let grade_dir: std::path::PathBuf = if let Some(ws) = workspace {
        ws.to_path_buf()
    } else {
        let handle_str = snapshot
            .map(str::to_string)
            .or_else(|| session.result_snapshot.clone())
            .ok_or_else(|| {
                PillboxError::usage(
                    "session score",
                    format!("session `{}` has no result_snapshot to grade", session.id),
                )
                .with_next(format!(
                    "pillbox session score {} --cmd \"…\" --workspace DIR",
                    session.id
                ))
            })?;
        let tmp = tempfile::tempdir()
            .map_err(|e| PillboxError::runtime("session score", format!("temp dir: {e}")))?;
        resolved
            .workspace()?
            .pull(tmp.path(), Some(&SnapshotHandle::new(handle_str)))?;
        let p = tmp.path().to_path_buf();
        _grade_tmp = Some(tmp);
        p
    };

    // The grader's exit status IS the verifiable grade — we run it + record what
    // it reports, never the agent's claim. `--in-sandbox` runs it in a one-shot
    // microVM (the runner toolchain) instead of on the host.
    let (code, feedback) = if in_sandbox {
        let (c, raw) = libkrun_score_in_sandbox(resolved, &grade_dir, cmd, grader_egress)?;
        (c, score_feedback(raw.as_bytes(), &[]))
    } else {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&grade_dir)
            .output()
            .map_err(|e| {
                PillboxError::runtime("session score", format!("run grader `{cmd}`: {e}"))
            })?;
        (
            out.status.code().unwrap_or(-1),
            score_feedback(&out.stdout, &out.stderr),
        )
    };
    let passed = code == 0;
    let score = if passed { 1.0 } else { 0.0 };

    // Record on the durable §0 log — the source of truth the meta-harness reads.
    let mut log = crate::events::log::SessionLog::open(resolved, &session.id)?;
    log.append(&[crate::contract::Event::session(
        &session.id,
        crate::contract::Payload::Scored(crate::contract::Scored {
            grader: cmd.to_string(),
            passed,
            score,
            feedback,
        }),
    )])?;

    let mark = if passed { "✓" } else { "✗" };
    println!(
        "pillbox: {mark} session `{}` scored {score:.2} ({})",
        session.id,
        if passed { "passed" } else { "failed" }
    );
    println!("  grader: {cmd}");
    Ok(())
}

/// Combine a grader's stdout+stderr into the `feedback` gradient, capped (keeping
/// the TAIL — pytest/cargo-test put the failure summary last) so one grade can't
/// bloat a §0 log line.
fn score_feedback(stdout: &[u8], stderr: &[u8]) -> String {
    const CAP: usize = 32 * 1024;
    let mut s = String::from_utf8_lossy(stdout).into_owned();
    let err = String::from_utf8_lossy(stderr);
    if !err.trim().is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&err);
    }
    if s.len() > CAP {
        let mut cut = s.len() - CAP;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s = format!("…[truncated {cut} leading bytes]\n{}", &s[cut..]);
    }
    s
}

fn session_prune(resolved: &Pillbox, dry_run: bool) -> Result<()> {
    // Single-pass classification: `Session::expiry_status` parses
    // `expires_at` once and returns a typed enum. We collect the
    // expired records and warn on malformed in the same loop — no
    // double scan, no two-predicate drift, no duplicate RFC3339
    // parse per session. Malformed records are left in place
    // (corrupt timestamp shouldn't silently drop user data).
    let mut expired: Vec<session::Session> = Vec::new();
    for s in session::list(resolved)? {
        match s.expiry_status() {
            session::ExpiryStatus::Malformed(value) => {
                eprintln!(
                    "pillbox: warning: session `{}` has malformed expires_at \
                     `{value}`; leaving in place (fix the record manually).",
                    s.id
                );
            }
            session::ExpiryStatus::Expired => expired.push(s),
            session::ExpiryStatus::Active | session::ExpiryStatus::NotSet => {}
        }
    }
    if expired.is_empty() {
        println!("pillbox: no sessions past their TTL — nothing to prune.");
        return Ok(());
    }
    if dry_run {
        println!(
            "pillbox: would prune {} session(s) (--dry-run):",
            expired.len()
        );
        for s in &expired {
            let exp = s.expires_at.as_deref().unwrap_or("(none)");
            println!("  {}  expires_at={exp}  remote={}", s.id, s.remote);
        }
        return Ok(());
    }
    // Drive `session rm` per record. Each call kills the sandbox and
    // deletes the local record. Errors are logged but don't abort the
    // loop — a single bad record shouldn't prevent pruning the rest.
    let mut pruned = 0usize;
    let mut failed = 0usize;
    for s in &expired {
        match session_rm(resolved, &s.id) {
            Ok(()) => pruned += 1,
            Err(e) => {
                eprintln!("pillbox: prune {}: {e}", s.id);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        eprintln!("pillbox: pruned {pruned}, {failed} failed (see above)");
    } else {
        println!("pillbox: ✓ pruned {pruned} session(s).");
    }
    Ok(())
}

fn session_started(resolved: &Pillbox, id: &str) -> Result<()> {
    validate_session_id(id)?;
    // Both PARENT_SESSION_ID_ENV and SESSION_STARTED_AT_ENV come from
    // the wrapper's bash exports. Shape was validated at the host's
    // CLI boundary; the env hop is privileged so we trust the values.
    let parent_session_id = events::parent_session_id_from_env();
    let mut stub = session::Session::sandbox_stub(id);
    // Prefer the wrapper-captured timestamp so the sandbox-side
    // `started_at` matches what `session done` will use as
    // span.start_time — single wall-clock read, no skew. Direct CLI
    // invocations without the env keep the now() fallback baked into
    // `sandbox_stub`.
    if let Some(env_started_at) = events::session_started_at_from_env() {
        stub.started_at = env_started_at;
    }
    events::emit_session_event(
        resolved,
        events::EventType::SessionStarted { parent_session_id },
        id,
        Some(&stub),
    );
    println!("pillbox: ✓ session `{id}` started");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn session_done(
    resolved: &Pillbox,
    id: &str,
    status: DoneStatus,
    reason: Option<String>,
    exit_code: Option<i32>,
    trace_path: Option<String>,
    result_snapshot: Option<String>,
) -> Result<()> {
    // Defense-in-depth on the id arg. `Session::new_id` produces 12
    // ascii-hex chars; the registry-resolve path enforces this via
    // length checks + filename lookup. The sandbox-side stub path
    // (no registry record) skips that gate, so the id flows straight
    // into the event payload and gets printed to stdout. Reject
    // anything that isn't ascii-alphanumeric (or `-` for forward-compat
    // with longer ids) and bound the length so a hostile orchestrator
    // can't make us emit a multi-megabyte "id" field.
    validate_session_id(id)?;
    // Two paths sharing a CLI surface:
    //   - host-side: registry has the record. Persist
    //     `--result-snapshot` onto it so later `session pull <id>`
    //     can rehydrate without round-tripping through the event
    //     stream.
    //   - sandbox-side: no record locally (the host owns it). Persist
    //     is skipped; the event still fires with the snapshot in the
    //     payload, and an orchestrator's webhook listener can call
    //     `session done` on the host to update the registry there.
    //
    // `Option<Session>` carries the host-vs-sandbox distinction
    // through the event-emit boundary cleanly — no empty-string
    // stub-detection hack, no "is this real?" probes downstream.
    let mut record = session::read(resolved, id)?;
    if let (Some(snap), Some(s)) = (&result_snapshot, &mut record) {
        s.result_snapshot = Some(snap.clone());
        // Best-effort: a write failure here doesn't void the event.
        if let Err(e) = session::write(resolved, s) {
            eprintln!("pillbox: warning: result_snapshot not persisted to session record: {e}");
        }
    }
    let event = match status {
        DoneStatus::Ok => events::EventType::SessionCompleted {
            exit_code,
            trace_path,
            result_snapshot,
        },
        DoneStatus::Failed => events::EventType::SessionFailed {
            reason: reason.unwrap_or_else(|| "agent failed".to_string()),
            exit_code,
            trace_path,
            result_snapshot,
        },
    };
    events::emit_session_event(resolved, event, id, record.as_ref());
    let label = match status {
        DoneStatus::Ok => "completed",
        DoneStatus::Failed => "failed",
    };
    println!("pillbox: ✓ session `{id}` marked {label}");
    Ok(())
}

/// Validate a session id passed on the command line. Accepts the
/// 12-hex-char form `Session::new_id` mints today, and tolerates up
/// to 64 chars of `[A-Za-z0-9-]` for forward compatibility with
/// longer ids. Anything outside that bucket is a usage error.
///
/// Public to the crate so `pillbox run --parent <id>` can apply the
/// same shape check at its CLI boundary before stashing the value in
/// `PILLBOX_PARENT_SESSION_ID`.
pub(crate) fn validate_session_id(id: &str) -> Result<()> {
    const MAX_LEN: usize = 64;
    if id.is_empty() {
        return Err(PillboxError::usage("session", "session id is empty").into());
    }
    if id.len() > MAX_LEN {
        return Err(PillboxError::usage(
            "session",
            format!("session id is {} chars, max {MAX_LEN}", id.len()),
        )
        .into());
    }
    if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(PillboxError::usage(
            "session",
            format!("session id `{id}` contains characters outside [A-Za-z0-9-]"),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_session_id_accepts_minted_form() {
        // `Session::new_id` produces 12 hex chars.
        validate_session_id("abcdef012345").unwrap();
    }

    #[test]
    fn score_feedback_combines_streams() {
        let f = score_feedback(b"out", b"err");
        assert_eq!(f, "out\nerr");
        assert_eq!(score_feedback(b"only-out", b""), "only-out");
        assert_eq!(score_feedback(b"", b"only-err"), "only-err");
    }

    #[test]
    fn score_feedback_keeps_the_tail_when_capped() {
        let big = "x".repeat(40 * 1024) + "TAIL-VERDICT";
        let f = score_feedback(big.as_bytes(), b"");
        assert!(f.starts_with("…[truncated"), "{}", &f[..40]);
        assert!(f.ends_with("TAIL-VERDICT"), "tail kept");
        assert!(f.len() < 34 * 1024, "capped near 32K, got {}", f.len());
    }

    #[test]
    fn validate_session_id_rejects_empty_and_too_long() {
        assert!(validate_session_id("").is_err());
        let too_long = "a".repeat(65);
        assert!(validate_session_id(&too_long).is_err());
    }

    #[test]
    fn validate_session_id_rejects_shell_metacharacters() {
        // The id flows into the sandbox-side shell wrapper line. Any
        // of these would be a problem (or at minimum, weird) — reject
        // at the host CLI rather than relying on shellEscape alone.
        for bad in [
            "abc def",
            "abc;rm -rf /",
            "abc$IFS",
            "abc'foo",
            "abc`whoami`",
            "../../../../etc/passwd",
            "abc\n",
        ] {
            assert!(validate_session_id(bad).is_err(), "should reject `{bad}`");
        }
    }
}
