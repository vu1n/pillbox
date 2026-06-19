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
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{events, sandbox, session};

mod grader;
mod stream;

/// Pid file a detached §0 producer ([`run_detached_tailer`]) writes in the session
/// dir, so teardown can SIGTERM it and live readers can tell a producer is keeping
/// the log fresh (and skip their own drain — the single-producer invariant).
pub(crate) const TAILER_PID_FILE: &str = ".tailer.pid";

/// The detached §0 PRODUCER for a reparented server session (the libkrun analog of
/// docker's always-on transcript tailer). Re-exec'd as a bare subprocess at
/// bring-up (`pillbox __session-tailer <dir> <capture> <format> <sid>`), it tails
/// the guest's persistent capture file → maps → appends to the durable log
/// FOREVER (until SIGTERM on teardown). This keeps the log continuously live for a
/// reparented agent the CLI doesn't supervise, so EVERY consumer — `list`/
/// `diagnose`/`subscribe` and the webhook/OTLP exporters — reads fresh data with
/// no explicit drain. Takes paths (not a `Pillbox`) since the child has no cwd
/// context. Sole producer: `subscribe`/`ingest` defer while it's alive.
pub(crate) fn run_detached_tailer(
    session_dir: std::path::PathBuf,
    capture: std::path::PathBuf,
    format: events::EventsFormat,
    sid: String,
) -> Result<()> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    // Claim sole-producer: our pid lets teardown stop us and signals readers a
    // producer is live (so they don't double-drain into the log).
    let _ = std::fs::write(
        session_dir.join(TAILER_PID_FILE),
        std::process::id().to_string(),
    );
    let mut log = events::log::SessionLog::open_at(session_dir)?;
    // `stop` is never set in-process — the producer runs until the process is
    // SIGTERM'd by `kill_session`. FollowReader blocks waiting for appends, so
    // the drain naturally idles when the agent is quiet and resumes on activity.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = events::opencode::FollowReader::new(capture, Arc::clone(&stop));
    events::drain_server_capture(format, reader, &sid, &mut log, &stop)?;
    Ok(())
}

/// The pid of a session's detached §0 producer, if its pid file is present, parseable, and positive.
/// THE one reader of the `.tailer.pid` contract — `detached_tailer_alive` (signal-0 probe) and
/// `kill_session` (SIGTERM on teardown) both go through it, so the format lives in one place.
// The detached §0 producer is a libkrun-server affordance, so the consumers
// (`LibkrunLiveSession::{event_source,ingest}` + `kill_session`) are all under
// the `libkrun` feature; dead on a build without it.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn tailer_pid(session_dir: &std::path::Path) -> Option<i32> {
    let pid = std::fs::read_to_string(session_dir.join(TAILER_PID_FILE))
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()?;
    (pid > 0).then_some(pid)
}

/// Is a detached §0 producer currently alive for this session? Pid file + a signal-0 liveness probe.
/// Readers (`subscribe`/`ingest`) use it to DEFER their own drain — only one writer may append to the
/// log or events double.
#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn detached_tailer_alive(resolved: &Pillbox, s: &session::Session) -> bool {
    tailer_pid(&session::session_dir_path(resolved, &s.id))
        .is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
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

pub(crate) fn dispatch(resolved: &Pillbox, action: SessionAction) -> Result<()> {
    match action {
        SessionAction::List { json } => session_list(resolved, json),
        SessionAction::Info { id, json } => session_info(resolved, &id, json),
        SessionAction::Diagnose { id, json } => session_diagnose(resolved, &id, json),
        SessionAction::Attach { id } => session_attach(resolved, &id),
        SessionAction::Detach { id } => session_detach(resolved, &id),
        SessionAction::Rm { id } => session_rm(resolved, &id),
        SessionAction::Send { id, text } => session_send(resolved, &id, &text),
        SessionAction::Annotate { id, text, anchor } => {
            session_annotate(resolved, &id, &text, anchor.as_deref())
        }
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
            rubric,
            snapshot,
            workspace,
            in_sandbox,
            grader_egress,
            json,
        } => session_score(
            resolved,
            &id,
            cmd.as_deref(),
            rubric.as_deref(),
            snapshot.as_deref(),
            workspace.as_deref(),
            in_sandbox,
            &grader_egress,
            json,
        ),
        SessionAction::Ingest { id, json } => session_ingest(resolved, &id, json),
        SessionAction::Log {
            id,
            r#type,
            last,
            from,
        } => session_log(resolved, &id, &r#type, last, from.unwrap_or(0)),
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
        SessionAction::Artifact { action } => session_artifact(resolved, action),
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
        println!("Start one with: pillbox run --detach");
        return Ok(());
    }
    println!(
        "Sessions in `{}` (id        status       attached?  agent    started_at):",
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
            "  {}  {:<11}  {attached}  {:<7}  {}{label}",
            s.id,
            status.label(),
            s.agent_id,
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
        // from this surface instead of parsing the session record. A backend with
        // no recoverable host workspace reports it unsupported → omit the field.
        let workspace = sandbox::live_session(&s)
            .ok()
            .and_then(|ls| ls.workspace_path().ok())
            .map(|p| p.to_string_lossy().into_owned());
        if let (Some(obj), Some(ws)) = (v.as_object_mut(), workspace) {
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
    println!("  backend:      {}", s.backend);
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
    sandbox::live_session(&s)?.attach(resolved)
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
    // A backend this binary can construct → tear down its sandbox via the plane.
    // A backend it can't (an unknown/removed remote backend, or a libkrun record
    // on a build without the feature) can't have its sandbox reached, but the
    // orphaned record must still be removable — drop it with a warning.
    match sandbox::live_session(&s) {
        Ok(live) => live.kill(resolved),
        Err(_) => {
            eprintln!(
                "pillbox: warning: session `{}` has an unsupported backend `{}` \
                 (this binary can't reach its sandbox); dropping the orphaned record only.",
                s.id, s.backend
            );
            session::delete(resolved, &s.id)?;
            println!("pillbox: ✓ session `{}` removed.", s.id);
            Ok(())
        }
    }
}

fn session_send(resolved: &Pillbox, id: &str, text: &str) -> Result<()> {
    // Drive targets a RUNNING session (a pty-host to write to), so resolve
    // against the session registry — not `resolve_logged` (the read side's
    // foreground log dirs).
    let s = session::resolve(resolved, id)?;
    // Drive through the one polymorphic `send`: each backend delivers the input the
    // way its agent expects — a structured prompt over the agent's HTTP API (or the
    // managed `/input`) for a server agent (opencode/codex-serve), raw keystrokes
    // for a PTY agent. The command layer no longer branches on integration or
    // backend; a backend that can't drive rejects with the standard unsupported
    // shape.
    sandbox::live_session(&s)?.send(text.as_bytes())?;
    let target = if s.integration() == Integration::Server {
        crate::contract::InputTarget::Agent
    } else {
        crate::contract::InputTarget::Pty
    };
    record_input(resolved, &s.id, text, target);
    match target {
        crate::contract::InputTarget::Agent => {
            eprintln!("pillbox: sent prompt to session `{}`", s.id)
        }
        _ => eprintln!("pillbox: sent {} byte(s) to session `{}`", text.len(), s.id),
    }
    Ok(())
}

/// Record `session send` as a durable, attributed §0 [`Input`](crate::contract::Input)
/// event (actor = the local human driver) so the log shows who drove + replays the
/// steer. Best-effort + loud: the steer already succeeded, so a §0 logging failure
/// is a warning, not a command failure.
///
/// CAVEAT (the seq-authority fault line): this opens a *fresh* [`SessionLog`], which
/// recovers `last_seq` from the file — a SECOND writer. If a `subscribe`/`watch`
/// tailer is concurrently appending agent output to the same `log.jsonl` from
/// another process (the Docker driven-session workflow), the two can assign the
/// same `seq` (no cross-process lock), degrading a subscriber's `--from`/resume
/// (dup/skip) — bytes stay intact (whole-line `O_APPEND`). libkrun is safe (one
/// detached producer). The real fix is single-writer coordination / a resident
/// sequencer (the deferred EventLog work); acceptable best-effort until then.
fn record_input(
    resolved: &Pillbox,
    session_id: &str,
    text: &str,
    target: crate::contract::InputTarget,
) {
    use crate::contract::{Actor, Event, Input, Payload};
    let ev = Event::session(
        session_id,
        Payload::Input(Input {
            text: text.to_string(),
            target,
        }),
    )
    .with_actor(Actor::human(local_user()));
    match crate::events::log::SessionLog::open(resolved, session_id) {
        Ok(mut log) => {
            if let Err(e) = log.append(&[ev]) {
                eprintln!("pillbox: warning: couldn't record input on the §0 log: {e:#}");
            }
        }
        Err(e) => eprintln!("pillbox: warning: couldn't open the §0 log to record input: {e:#}"),
    }
}

/// Record an attributed §0 [`Annotation`](crate::contract::Annotation) — the async,
/// keyboard-free "chime in" that does NOT drive the agent (unlike `session send`).
/// Stamped `human(<os user>)`; an orchestrator may later inject it as agent context.
/// Hard-errors on append failure: recording the annotation IS this command's job
/// (vs `record_input`, a side-effect of an already-succeeded send). Shares
/// `record_input`'s single-writer seq caveat (a second `SessionLog` opener) —
/// fine locally; the managed/multiplayer path sequences through the resident DO.
fn session_annotate(resolved: &Pillbox, id: &str, text: &str, anchor: Option<&str>) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    let ev = crate::contract::Event::session(
        &s.id,
        crate::contract::Payload::Annotation(crate::contract::Annotation {
            text: text.to_string(),
            anchor: anchor.unwrap_or_default().to_string(),
        }),
    )
    .with_actor(crate::contract::Actor::human(local_user()));
    crate::events::log::SessionLog::open(resolved, &s.id)?.append(&[ev])?;
    eprintln!("pillbox: annotated session `{}`", s.id);
    Ok(())
}

/// The local human identity for §0 actor attribution — the OS user, else `local`.
fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "local".into())
}

fn session_pull(resolved: &Pillbox, id: &str, to: Option<&std::path::Path>) -> Result<()> {
    let session = session::resolve(resolved, id)?;
    let target = match to {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| PillboxError::runtime("session pull", format!("resolve cwd: {e}")))?
            .join(format!("session-{}", session.id)),
    };
    match rehydrate_result(resolved, &session, &target, "session pull")? {
        RehydrateSource::Snapshot(handle) => println!(
            "pillbox: ✓ restored session `{}` (snapshot `{}`) into {}",
            session.id,
            handle.short(),
            target.display()
        ),
        RehydrateSource::LiveClone => println!(
            "pillbox: ✓ restored session `{}` (live workspace) into {}",
            session.id,
            target.display()
        ),
    }
    Ok(())
}

/// Which source [`rehydrate_result`] recovered a session's result tree from.
pub(crate) enum RehydrateSource {
    /// The agent's `result_snapshot` from the rustic repo (canonical; survives
    /// `session rm`). Carries the handle that was restored.
    Snapshot(crate::workspace::SnapshotHandle),
    /// The live backend workspace clone (a libkrun headless session that nothing
    /// snapshots). Only available while the session is alive.
    LiveClone,
}

/// Rehydrate a finished session's result tree into `target` (created if
/// missing), then report which source it came from. No stdout — callers format
/// their own banner. The shared core behind `session pull` (one session) and
/// `collect` (a batch). `action` is the user-facing command label stamped into
/// any error (so a `collect` failure reports "collect failed", not "session
/// pull failed").
///
/// Two recovery sources, in priority order:
///   1. `result_snapshot` — the agent pushed its result tree into the rustic
///      repo (set by `session done --result-snapshot`). Snapshot-backed,
///      survives `session rm`. The canonical path.
///   2. The live backend workspace clone — a libkrun server/detached session
///      runs headless against a CoW clone that nothing ever snapshots (no
///      in-sandbox wrapper calls `session done` for it), so its edits would be
///      stranded on disk until teardown scrubs the clone. While the session is
///      alive (it lives until `session rm`) copy them out directly. No rustic
///      push — this works for the global pillbox too (which owns no repo).
pub(crate) fn rehydrate_result(
    resolved: &Pillbox,
    session: &session::Session,
    target: &std::path::Path,
    action: &'static str,
) -> Result<RehydrateSource> {
    use crate::workspace::{SnapshotHandle, WorkspaceBackend};
    if let Some(handle_str) = session.result_snapshot.as_ref() {
        let backend = resolved.workspace()?;
        std::fs::create_dir_all(target)
            .map_err(|e| PillboxError::runtime(action, format!("create {target:?}: {e}")))?;
        let handle = SnapshotHandle::new(handle_str.clone());
        backend.pull(target, Some(&handle))?;
        return Ok(RehydrateSource::Snapshot(handle));
    }

    if let Some(clone) = live_workspace_clone(session) {
        std::fs::create_dir_all(target)
            .map_err(|e| PillboxError::runtime(action, format!("create {target:?}: {e}")))?;
        copy_dir_into(&clone, target).map_err(|e| {
            PillboxError::runtime(
                action,
                format!("copy live workspace {}: {e}", clone.display()),
            )
        })?;
        return Ok(RehydrateSource::LiveClone);
    }

    Err(PillboxError::runtime(
        action,
        format!(
            "session `{}` has no result to pull — no result_snapshot and no \
             live workspace clone (the agent hasn't finished, or the session \
             was already torn down)",
            session.id
        ),
    )
    .with_next(format!("pillbox session info {}", session.id))
    .into())
}

/// The on-disk path of a session's live backend workspace clone, if the backend
/// keeps one (libkrun: the CoW clone the guest mounts and the agent edits). Only
/// returned when the directory still exists — a torn-down session's clone is
/// gone. `None` for backends without a host-visible result tree.
pub(crate) fn live_workspace_clone(session: &session::Session) -> Option<std::path::PathBuf> {
    let path = sandbox::live_session(session)
        .ok()
        .and_then(|ls| ls.workspace_path().ok())?;
    path.is_dir().then_some(path)
}

/// Recursively copy the contents of `src` into the existing directory `dst`
/// (entries land at `dst/<name>`, not under a `src`-named subdir). Symlinks are
/// recreated as links; everything else is byte-copied. Backs `session pull`'s
/// live-clone fallback — the clone is already secret-scrubbed at creation
/// (`cow_clone_and_scrub`), so there's nothing to re-filter here.
fn copy_dir_into(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            let link_target = std::fs::read_link(&from)?;
            // Replace any existing entry so a re-pull into the same dir is idempotent.
            let _ = std::fs::remove_file(&to);
            std::os::unix::fs::symlink(link_target, &to)?;
        } else if ft.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir_into(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
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

    // Post-hoc drain through the plane. Only a backend with a headless capture file
    // supports it (libkrun server agents); others reject — they drain live through
    // `session subscribe`/`watch`. The impl also defers to a live detached §0
    // producer (returns 0 rather than double-writing the log).
    let n = sandbox::live_session(&session)?.ingest(resolved)?;
    // The §0 log is now drained — if this was a `--memory` server run, capture it into kypp + record
    // briefed-claim usage (the brief the bring-up stashed). No-op otherwise. Before the marker so a
    // failed capture can retry on a re-ingest; the marker then makes the whole step run once.
    if let Ok(dir) = session::session_dir(resolved, &session.id) {
        crate::memory::capture_session(&dir, &session.id);
    }
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

/// Read a session's durable §0 log (`log.jsonl`), filtered by payload `--type`
/// and/or trimmed to the `--last` match, as JSONL on stdout — the structured §0
/// read that replaces opening the on-disk log by hand. Filtering is on the
/// serialized payload `type` tag (not a 25-arm enum match that would drift from
/// `Payload`); a missing log reads as empty (see [`SessionLog::read_from`]).
fn session_log(
    resolved: &Pillbox,
    id: &str,
    types: &[String],
    last: bool,
    from: u64,
) -> Result<()> {
    use std::io::Write;

    let session = session::resolve(resolved, id)?;
    let log = crate::events::log::SessionLog::open(resolved, &session.id)?;
    let matched = select_log_events(log.read_from(from)?, types, last)?;

    // One writer, one flush — a large trajectory dump shouldn't pay a syscall
    // per line. BrokenPipe (downstream `head`/`grep -q` closed early) is a clean
    // exit, not an error.
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for v in &matched {
        if let Err(e) = writeln!(out, "{v}") {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(PillboxError::runtime("session log", format!("write: {e}")).into());
        }
    }
    out.flush().ok();
    Ok(())
}

/// Serialize + filter §0 events for `session log`: keep those whose payload
/// `type` is in `types` (empty = keep all), then trim to the last match if
/// `last`. Pure (no I/O) so the `--type`/`--last` semantics are unit-testable.
fn select_log_events(
    events: Vec<crate::contract::Event>,
    types: &[String],
    last: bool,
) -> Result<Vec<serde_json::Value>> {
    let want: Option<std::collections::HashSet<&str>> = if types.is_empty() {
        None
    } else {
        Some(types.iter().map(String::as_str).collect())
    };

    let mut matched: Vec<serde_json::Value> = Vec::new();
    for ev in events {
        let v = serde_json::to_value(&ev)
            .map_err(|e| PillboxError::runtime("session log", format!("serialize event: {e}")))?;
        if let Some(want) = &want {
            // Match on the serialized payload `type` tag. A foreign/forward
            // payload lands as `Payload::Unknown`, which re-serializes as
            // `{"type":"unknown"}` — so `--type unknown` deliberately selects it;
            // a missing tag (shouldn't happen) is unmatchable.
            match v
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(serde_json::Value::as_str)
            {
                Some(t) if want.contains(t) => {}
                _ => continue,
            }
        }
        matched.push(v);
    }
    if last && matched.len() > 1 {
        matched.drain(..matched.len() - 1);
    }
    Ok(matched)
}

/// `session artifact …` — write/read a structured artifact + its blob.
fn session_artifact(resolved: &Pillbox, action: crate::cli::ArtifactAction) -> Result<()> {
    use crate::cli::ArtifactAction;
    match action {
        ArtifactAction::Put {
            id,
            kind,
            summary,
            content_type,
            class,
            worker,
            file,
            json,
        } => session_artifact_put(
            resolved,
            &id,
            &kind,
            summary.as_deref(),
            content_type.as_deref(),
            class,
            worker.as_deref(),
            file.as_deref(),
            json,
        ),
        ArtifactAction::Get { id, r#ref } => session_artifact_get(resolved, &id, &r#ref),
    }
}

/// Store an artifact body (from `--file` or stdin) in the session's content-
/// addressed blob store and append an `artifact` §0 event referencing it — the
/// host-side hook a grader / judge / explorer uses to attach output without
/// inlining it into the transcript. The blob is content-addressed (idempotent),
/// the log line stays small (a typed ref), and the body is fetched back with
/// `session artifact get --ref`. Stamped `service:artifact` (the generic CLI
/// producer); an in-process producer like dispatch stamps its own actor.
#[allow(clippy::too_many_arguments)] // a CLI leaf handler — args mirror parsed flags 1:1
fn session_artifact_put(
    resolved: &Pillbox,
    id: &str,
    kind: &str,
    summary: Option<&str>,
    content_type: Option<&str>,
    class: crate::cli::ArtifactClassArg,
    worker: Option<&str>,
    file: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    use std::io::Read;

    if kind.trim().is_empty() {
        return Err(PillboxError::usage("session artifact put", "--kind must not be empty").into());
    }
    let session = session::resolve(resolved, id)?;

    // Body from --file, else stdin (the pipe shape: `… | pillbox session artifact put`).
    let body = match file {
        Some(p) => std::fs::read(p).map_err(|e| {
            PillboxError::runtime("session artifact put", format!("read {}: {e}", p.display()))
        })?,
        None => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf).map_err(|e| {
                PillboxError::runtime("session artifact put", format!("read stdin: {e}"))
            })?;
            buf
        }
    };

    let store = crate::events::blob::BlobStore::open(resolved, &session.id)?;
    let blob_ref = store.put(&body)?;
    let artifact = crate::contract::Artifact {
        kind: kind.to_string(),
        summary: summary.unwrap_or_default().to_string(),
        content_type: content_type.unwrap_or_default().to_string(),
        class: match class {
            crate::cli::ArtifactClassArg::Content => crate::contract::ArtifactClass::Content,
            crate::cli::ArtifactClassArg::Signal => crate::contract::ArtifactClass::Signal,
        },
        blob_ref: blob_ref.clone(),
        bytes: body.len() as u64,
        worker_id: worker.unwrap_or_default().to_string(),
    };

    let mut log = crate::events::log::SessionLog::open(resolved, &session.id)?;
    let seq = log.append(&[crate::contract::Event::session(
        &session.id,
        crate::contract::Payload::Artifact(artifact.clone()),
    )
    .with_actor(crate::contract::Actor::service("artifact"))])?;

    if json {
        println!("{}", artifact_ref_json(&session.id, &artifact, seq));
    } else {
        println!(
            "pillbox: ✓ artifact `{}` ({} bytes) → seq {seq}  ref {}",
            artifact.kind, artifact.bytes, artifact.blob_ref
        );
    }
    Ok(())
}

/// Read an artifact body by blob ref and write it to stdout — the lazy
/// dereference of a `blobRef` seen in `session log --type artifact`. Resolves
/// the session record (consistent with `score`/`log`), validates the ref is a
/// bare sha256 handle (the traversal guard lives in `BlobStore`), then streams
/// the bytes; a broken downstream pipe is a clean exit.
fn session_artifact_get(resolved: &Pillbox, id: &str, blob_ref: &str) -> Result<()> {
    use std::io::Write;

    let session = session::resolve(resolved, id)?;
    let store = crate::events::blob::BlobStore::open(resolved, &session.id)?;
    let body = store.get(blob_ref)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match out.write_all(&body).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(PillboxError::runtime("session artifact get", format!("write: {e}")).into()),
    }
}

/// The `artifact put --json` reference envelope — the artifact event body plus
/// a `{version, session, seq}` wrapper, so a caller (the eval loop, an
/// orchestrator) reads the ref directly without scraping the §0 log. serde-built
/// from the `Artifact` itself so the `--json` surface can't drift from the
/// logged event (and `skip_serializing_if` rules carry through).
fn artifact_ref_json(
    session_id: &str,
    artifact: &crate::contract::Artifact,
    seq: u64,
) -> serde_json::Value {
    let mut v = serde_json::to_value(artifact).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("version".into(), serde_json::json!(1));
        obj.insert("session".into(), serde_json::json!(session_id));
        obj.insert("seq".into(), serde_json::json!(seq));
    }
    v
}

/// Externally grade a session's result and record it as a verifiable `scored`
/// §0 event — the reward channel the optimization loops gate on. Runs the grader
/// (via `sh -c`) with cwd = the graded workspace: `workspace` if given, else a
/// rehydrated snapshot (`snapshot`, or the session's `result_snapshot`). The
/// grader's **exit status** is the verifiable pass/fail (NOT the agent's
/// `session done` self-report); its combined output is the feedback gradient.
/// `--cmd` is one command; `--rubric FILE` is N named criteria → per-criterion
/// verdicts + a fractional score. `--in-sandbox` runs it in a one-shot microVM.
#[allow(clippy::too_many_arguments)] // a CLI leaf handler — args mirror parsed flags 1:1
fn session_score(
    resolved: &Pillbox,
    id: &str,
    cmd: Option<&str>,
    rubric: Option<&std::path::Path>,
    snapshot: Option<&str>,
    workspace: Option<&std::path::Path>,
    in_sandbox: bool,
    grader_egress: &[String],
    json: bool,
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

    // Resolve the grader (one --cmd, or a --rubric file parsed + compiled to one
    // script) — the marker protocol + scoring policy lives in `grader`.
    let spec = grader::GraderSpec::resolve(cmd, rubric)?;
    let exec_cmd = spec.exec_command();
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
    // it reports, never the agent's claim. Keep the output RAW (uncapped): a
    // rubric's frame markers are interspersed, so capping here (tail-only) could
    // drop early criteria. `--in-sandbox` runs it in a one-shot microVM.
    let (code, raw) = if in_sandbox {
        libkrun_score_in_sandbox(resolved, &grade_dir, &exec_cmd, grader_egress)?
    } else {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&exec_cmd)
            .current_dir(&grade_dir)
            .output()
            .map_err(|e| PillboxError::runtime("session score", format!("run grader: {e}")))?;
        (
            out.status.code().unwrap_or(-1),
            grader::combine_streams(&out.stdout, &out.stderr),
        )
    };

    // Score the (exit, output) into a verdict — binary for --cmd, fractional +
    // per-criterion for --rubric (the policy lives in `grader`).
    let result = grader::grade_result(&spec, code, raw);
    let scored = crate::contract::Scored {
        grader: spec.label(),
        passed: result.passed,
        score: result.score,
        feedback: result.feedback,
        criteria: result.criteria,
    };

    // Record on the durable §0 log — the source of truth the meta-harness reads.
    let mut log = crate::events::log::SessionLog::open(resolved, &session.id)?;
    // The grade is an external verifier's verdict, not the agent's — stamp `service`.
    let seq = log.append(&[crate::contract::Event::session(
        &session.id,
        crate::contract::Payload::Scored(scored.clone()),
    )
    .with_actor(crate::contract::Actor::service("grader"))])?;

    if json {
        println!("{}", score_verdict_json(&session.id, &scored, seq));
    } else {
        let mark = if scored.passed { "✓" } else { "✗" };
        println!(
            "pillbox: {mark} session `{}` scored {:.2} ({})",
            session.id,
            scored.score,
            if scored.passed { "passed" } else { "failed" }
        );
        println!("  grader: {}", scored.grader);
        // List each criterion for a rubric grade — the at-a-glance "which failed".
        for c in &scored.criteria {
            println!("  {} {}", if c.passed { "✓" } else { "✗" }, c.name);
        }
    }
    Ok(())
}

/// The `score --json` verdict — the structured result a caller reads instead of
/// scraping stdout or the §0 log. The verdict body IS the `Scored` event —
/// serialize it once (so the `--json` surface and the §0 log event can't diverge,
/// and `Scored`'s `skip_serializing_if` rules carry through, e.g. empty `criteria`
/// omitted), then add the `version`/`session`/`seq` envelope. serde-built (not a
/// format string), so arbitrary `feedback` is escaped correctly. `seq` is the
/// appended event's log seq, for a §0 follow-up.
fn score_verdict_json(
    session_id: &str,
    scored: &crate::contract::Scored,
    seq: u64,
) -> serde_json::Value {
    let mut v = serde_json::to_value(scored).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("version".into(), serde_json::json!(1));
        obj.insert("session".into(), serde_json::json!(session_id));
        obj.insert("seq".into(), serde_json::json!(seq));
    }
    v
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
            println!("  {}  expires_at={exp}  backend={}", s.id, s.backend);
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
        events::EventType::SessionStarted {
            parent_session_id,
            startup: None,
        },
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
    fn copy_dir_into_recreates_tree_files_and_symlinks() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // file at root, nested file, and a symlink to the root file.
        std::fs::write(src.path().join("a.txt"), b"alpha").unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/b.txt"), b"bravo").unwrap();
        std::os::unix::fs::symlink("a.txt", src.path().join("link")).unwrap();

        copy_dir_into(src.path(), dst.path()).unwrap();

        // Contents land directly under dst (not under a src-named subdir).
        assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"alpha");
        assert_eq!(
            std::fs::read(dst.path().join("sub/b.txt")).unwrap(),
            b"bravo"
        );
        // The symlink is recreated as a link (not dereferenced into a copy).
        let meta = std::fs::symlink_metadata(dst.path().join("link")).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(dst.path().join("link")).unwrap(),
            std::path::Path::new("a.txt")
        );
    }

    #[test]
    fn copy_dir_into_is_idempotent_on_rerun() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), b"v1").unwrap();
        std::os::unix::fs::symlink("a.txt", src.path().join("link")).unwrap();

        copy_dir_into(src.path(), dst.path()).unwrap();
        // A second pull into the same dir must overwrite cleanly, not error on the
        // pre-existing symlink (the live-clone fallback re-pull case).
        std::fs::write(src.path().join("a.txt"), b"v2").unwrap();
        copy_dir_into(src.path(), dst.path()).unwrap();

        assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"v2");
        assert!(std::fs::symlink_metadata(dst.path().join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn record_input_writes_an_attributed_input_event() {
        crate::test_util::with_isolated_home("session-record-input", || {
            let pb = crate::pillbox::global();
            record_input(
                &pb,
                "sess-drive",
                "fix it",
                crate::contract::InputTarget::Agent,
            );
            let events = crate::events::log::SessionLog::open(&pb, "sess-drive")
                .unwrap()
                .read_from(0)
                .unwrap();
            assert_eq!(events.len(), 1);
            let ev = &events[0];
            assert!(matches!(&ev.payload, crate::contract::Payload::Input(i)
                if i.text == "fix it" && i.target == crate::contract::InputTarget::Agent));
            let actor = ev.actor.as_ref().expect("input is actor-stamped");
            assert_eq!(actor.kind, crate::contract::ActorKind::Human);
            assert!(
                actor.id.starts_with("u:"),
                "human id is kind-prefixed: {}",
                actor.id
            );
        });
    }

    #[test]
    fn artifact_put_get_round_trips_through_a_session() {
        crate::test_util::with_isolated_home("session-artifact-roundtrip", || {
            use crate::contract::{ActorKind, ArtifactClass, Payload};
            let pb = crate::pillbox::global();
            // A resolvable session record — the put/get handlers call `session::resolve`.
            let s = session::Session::test_fixture();
            session::write(&pb, &s).unwrap();

            // Producer: write an artifact body from a file.
            let tmp = tempfile::tempdir().unwrap();
            let body = br#"{"citations":[{"file":"src/auth.rs","line":42}]}"#;
            let f = tmp.path().join("cite.json");
            std::fs::write(&f, body).unwrap();
            session_artifact_put(
                &pb,
                &s.id,
                "code_explore.citations",
                Some("2 hits"),
                Some("application/json"),
                crate::cli::ArtifactClassArg::Signal,
                Some("w1"),
                Some(f.as_path()),
                false,
            )
            .unwrap();

            // The log now carries one `artifact` event with the typed ref...
            let events = crate::events::log::SessionLog::open(&pb, &s.id)
                .unwrap()
                .read_from(0)
                .unwrap();
            let ev = events
                .iter()
                .find(|e| matches!(e.payload, Payload::Artifact(_)))
                .expect("artifact event on the log");
            let Payload::Artifact(art) = &ev.payload else {
                unreachable!()
            };
            assert_eq!(art.kind, "code_explore.citations");
            assert_eq!(art.summary, "2 hits");
            assert_eq!(art.class, ArtifactClass::Signal);
            assert_eq!(art.worker_id, "w1");
            assert_eq!(art.bytes, body.len() as u64);
            assert!(!art.blob_ref.is_empty());
            // ...stamped as the generic CLI producer (service), not the agent.
            assert_eq!(ev.actor.as_ref().unwrap().kind, ActorKind::Service);

            // Reader: the blob dereferences back to the exact bytes.
            let store = crate::events::blob::BlobStore::open(&pb, &s.id).unwrap();
            assert_eq!(store.get(&art.blob_ref).unwrap(), body);
        });
    }

    #[test]
    fn artifact_ref_json_is_the_documented_shape() {
        let art = crate::contract::Artifact {
            kind: "judge.report".into(),
            summary: "consensus: ship".into(),
            content_type: "text/plain".into(),
            class: crate::contract::ArtifactClass::Content,
            blob_ref: "deadbeef".into(),
            bytes: 128,
            worker_id: String::new(),
        };
        let v = artifact_ref_json("abc123", &art, 9);
        assert_eq!(v["version"], 1);
        assert_eq!(v["session"], "abc123");
        assert_eq!(v["seq"], 9);
        assert_eq!(v["kind"], "judge.report");
        assert_eq!(v["blobRef"], "deadbeef");
        assert_eq!(v["bytes"], 128);
        assert_eq!(v["class"], "content");
        // Empty worker_id is omitted (skip_serializing_if carries through).
        assert!(v.get("workerId").is_none());
    }

    fn scored(grader: &str, passed: bool, score: f64, feedback: &str) -> crate::contract::Scored {
        crate::contract::Scored {
            grader: grader.into(),
            passed,
            score,
            feedback: feedback.into(),
            criteria: Vec::new(),
        }
    }

    #[test]
    fn score_verdict_json_is_the_documented_shape() {
        let v = score_verdict_json("abc123", &scored("pytest -q", true, 1.0, "5 passed"), 7);
        assert_eq!(v["version"], 1);
        assert_eq!(v["session"], "abc123");
        assert_eq!(v["grader"], "pytest -q");
        assert_eq!(v["passed"], true);
        assert_eq!(v["score"], 1.0);
        assert_eq!(v["feedback"], "5 passed");
        assert_eq!(v["seq"], 7);
        // No criteria for a plain --cmd grade — keep the legacy shape stable.
        assert!(v.get("criteria").is_none());
    }

    #[test]
    fn score_verdict_json_escapes_arbitrary_feedback() {
        // Grader output with quotes/newlines must round-trip as one JSON value —
        // the whole reason for serde over a format string.
        let nasty = "line1\n\"quoted\"\ttab \\ backslash";
        let v = score_verdict_json("s", &scored("g", false, 0.0, nasty), 1);
        let s = v.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["feedback"], nasty);
        assert_eq!(parsed["passed"], false);
    }

    #[test]
    fn score_verdict_json_includes_criteria_for_a_rubric() {
        let mut s = scored("rubric:r.txt", false, 0.5, "✗ tests pass");
        s.criteria = vec![crate::contract::Criterion {
            name: "tests pass".into(),
            passed: false,
            feedback: "1 failed".into(),
        }];
        let v = score_verdict_json("s", &s, 3);
        assert_eq!(v["criteria"][0]["name"], "tests pass");
        assert_eq!(v["criteria"][0]["passed"], false);
        assert_eq!(v["score"], 0.5);
    }

    #[test]
    fn select_log_events_filters_by_type_and_last() {
        use crate::contract::{Event, Payload, Scored, ToolCall, ToolStatus};
        let ev = |p: Payload| Event::session("s", p);
        let tool = |name: &str| {
            ev(Payload::ToolCall(ToolCall {
                tool_call_id: name.into(),
                name: name.into(),
                status: ToolStatus::Completed,
                input: None,
                output: String::new(),
                title: String::new(),
            }))
        };
        let scored = |s: f64| {
            ev(Payload::Scored(Scored {
                grader: "g".into(),
                passed: s >= 1.0,
                score: s,
                feedback: String::new(),
                criteria: Vec::new(),
            }))
        };
        let events = vec![tool("read"), scored(0.0), tool("edit"), scored(1.0)];

        // No filter → all four, in order.
        let all = select_log_events(events.clone(), &[], false).unwrap();
        assert_eq!(all.len(), 4);

        // --type tool_call → just the two tool calls.
        let tools = select_log_events(events.clone(), &["tool_call".into()], false).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["payload"]["name"], "read");

        // --type scored --last → only the final scored event.
        let last = select_log_events(events.clone(), &["scored".into()], true).unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0]["payload"]["score"], 1.0);

        // Unknown tag matches nothing (typo → empty, not error).
        assert!(select_log_events(events, &["nope".into()], false)
            .unwrap()
            .is_empty());
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
