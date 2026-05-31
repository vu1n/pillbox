//! `pillbox session …` handlers — list/info/attach/detach/rm/pull/
//! prune + the `started`/`done` event emitters used by the sandbox-
//! side wrapper. main.rs's `Command::Session { action }` arm calls
//! into [`dispatch`]; everything else here is private to the module.
//!
//! Lives in `commands/` (not the registry-storage `session.rs`) so
//! the CLI surface stays decoupled from the on-disk record format.

use anyhow::Result;

use crate::cli::{DoneStatus, SessionAction};
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{events, remote, sandbox, session};

/// A `Backend::Docker` session whose `remote` marks it local (the host daemon),
/// vs a `docker://` remote session (a remote name / inline URL to re-resolve).
/// Empty is treated as local for forward-compat with pre-`LOCAL_REMOTE` records.
fn is_local_docker(remote: &str) -> bool {
    remote == session::LOCAL_REMOTE || remote.is_empty()
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
            session_subscribe(resolved, &id, from, bind.as_deref())
        }
        SessionAction::Watch { id, from } => session_watch(resolved, &id, from),
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
                let mut v = s.to_json_value();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("status".into(), status.label().into());
                }
                Ok(v)
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
        let mut v = s.to_json_value();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("status".into(), status.label().into());
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
    use crate::contract::{Payload, Role, ToolStatus};

    let s = session::resolve(resolved, id)?;
    let terminals = events::status::terminal_outcomes(resolved)?;
    let terminal = terminals.get(&s.id);
    let status = events::status::derive(resolved, &s, terminal)?;

    // Activity summary, folded from the per-session durable log (read-only).
    let log = events::log::read_log(resolved, &s.id, 0)?;
    let seq = log.last().map(|e| e.seq).unwrap_or(0);
    let mut assistant_turns = 0u64;
    let mut tool_calls = 0u64;
    let mut last_at = String::new();
    for ev in &log {
        if !ev.at.is_empty() {
            last_at = ev.at.clone();
        }
        match &ev.payload {
            Payload::MessageStart(m) if m.role == Role::Assistant => assistant_turns += 1,
            // A tool call lands twice (Running, then its correlated result);
            // count the Running side so the number is invocations, not events.
            Payload::ToolCall(t) if matches!(t.status, ToolStatus::Running) => tool_calls += 1,
            _ => {}
        }
    }

    let (fail_reason, exit_code) = match terminal {
        Some(events::status::Terminal::Failed { reason, exit_code }) => {
            (Some(reason.clone()), *exit_code)
        }
        Some(events::status::Terminal::Done { exit_code }) => (None, *exit_code),
        None => (None, None),
    };

    if json {
        let mut v = s.to_json_value();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("status".into(), status.label().into());
            obj.insert("log_seq".into(), seq.into());
            obj.insert("assistant_turns".into(), assistant_turns.into());
            obj.insert("tool_calls".into(), tool_calls.into());
            if !last_at.is_empty() {
                obj.insert("last_event_at".into(), last_at.into());
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

    println!("Session {} — {}", s.id, status.label());
    if let Some(r) = &fail_reason {
        let suffix = exit_code
            .map(|c| format!(" (exit {c})"))
            .unwrap_or_default();
        println!("  failure:      {r}{suffix}");
    } else if let Some(c) = exit_code {
        println!("  exit_code:    {c}");
    }
    if status == events::status::SessionStatus::NeedsInput {
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
    if log.is_empty() {
        println!("  (no host-visible log — a remote session streams its transcript sandbox-side)");
    } else {
        println!(
            "  {assistant_turns} assistant turn(s), {tool_calls} tool call(s), {seq} event(s)"
        );
        if !last_at.is_empty() {
            println!("  last event at {last_at}");
        }
    }
    Ok(())
}

fn session_attach(resolved: &Pillbox, id: &str) -> Result<()> {
    let s = session::resolve(resolved, id)?;
    match session::Backend::parse(&s.backend) {
        // Both local and docker:// sessions are `Backend::Docker` (a container
        // either way); the `remote` field disambiguates. Local attaches to the
        // host daemon directly; docker:// re-resolves the endpoint.
        Some(session::Backend::Docker) if is_local_docker(&s.remote) => {
            sandbox::local_docker::reattach(resolved, &s)
        }
        Some(session::Backend::Docker) => {
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
        None => Err(PillboxError::config(
            "session attach",
            format!("unknown session backend `{}`", s.backend),
        )
        .into()),
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
        Some(session::Backend::Docker) if is_local_docker(&s.remote) => {
            sandbox::local_docker::kill_session(resolved, &s)
        }
        Some(session::Backend::Docker) => {
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
        None => Err(PillboxError::config(
            "session rm",
            format!("unknown session backend `{}`", s.backend),
        )
        .into()),
    }
}

fn session_send(resolved: &Pillbox, id: &str, text: &str) -> Result<()> {
    // Drive targets a RUNNING session (a pty-host to write to), so resolve
    // against the session registry — not `resolve_logged` (the read side's
    // foreground log dirs).
    let s = session::resolve(resolved, id)?;
    match session::Backend::parse(&s.backend) {
        Some(session::Backend::Docker) => {
            sandbox::local_docker::send_input(&s.sandbox_id, text.as_bytes())?;
            eprintln!("pillbox: sent {} byte(s) to session `{}`", text.len(), s.id);
            Ok(())
        }
        // The same relay-exec pattern extends to e2b/ssh (mirroring their
        // reattach transport); not wired yet.
        Some(_) => Err(PillboxError::usage(
            "session send",
            format!(
                "send isn't supported for `{}` sessions yet (local docker only)",
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

/// Resolve a session id/prefix for streaming and ensure its log is being
/// filled. A live local-docker record → spawn the transcript→log tailer
/// (returned as a guard the caller holds for the stream's lifetime, so a
/// `session send`-driven session is readable as it runs); a remote record →
/// note that live tailing is host-unavailable (transcript is sandbox-side); a
/// foreground/historical run → resolve the log dir. Shared by `session
/// subscribe` (serves WS) and `session watch` (renders to the terminal).
fn resolve_streaming_session(
    resolved: &Pillbox,
    id: &str,
    action: &'static str,
) -> Result<(String, Option<events::transcripts::LocalTailerHandle>)> {
    if let Ok(s) = session::resolve(resolved, id) {
        let tailer = match session::Backend::parse(&s.backend) {
            Some(session::Backend::Docker) => {
                let spec = crate::agents::lookup(action, &s.agent_id)?;
                let home = spec.home_dir(resolved)?;
                let log = crate::events::log::SessionLog::open(resolved, &s.id)?;
                events::transcripts::spawn_attach_tailer(
                    log,
                    &home,
                    &s.agent_id,
                    &s.guest_cwd,
                    &s.id,
                )
            }
            _ => {
                eprintln!(
                    "pillbox: note: live event tailing is local-docker only (a remote \
                     session's transcript is sandbox-side); reading the existing log"
                );
                None
            }
        };
        return Ok((s.id, tailer));
    }
    // Foreground/historical run: a durable LOG but no `.toml` record.
    Ok((session::resolve_logged(resolved, id)?, None))
}

fn session_subscribe(resolved: &Pillbox, id: &str, from: u64, bind: Option<&str>) -> Result<()> {
    let (sid, _tailer) = resolve_streaming_session(resolved, id, "session subscribe")?;
    // Read-side fan-out: while the log is being filled (live session), if a
    // notification webhook is configured, tail the log and POST attention
    // signals to it — a consumer of the log (its own read view), off the
    // tailer's producer path.
    if _tailer.is_some() {
        if let Some(url) = std::env::var("PILLBOX_EVENTS_WEBHOOK")
            .ok()
            .filter(|u| !u.is_empty())
        {
            if let Ok(elog) = crate::events::log::SessionLog::open(resolved, &sid) {
                crate::events::spawn_webhook_log_exporter(elog, url);
            }
        }
    }
    // `_tailer` lives for the gateway's lifetime (serve_session_ws blocks).
    crate::gateway::serve_session_ws(resolved, &sid, from, bind)
}

fn session_watch(resolved: &Pillbox, id: &str, from: u64) -> Result<()> {
    use std::sync::atomic::AtomicBool;
    let (sid, _tailer) = resolve_streaming_session(resolved, id, "session watch")?;
    eprintln!("pillbox: watching session {sid} (Ctrl-C to stop)");
    let log = crate::events::log::SessionLog::open(resolved, &sid)?;
    // Never set: Ctrl-C ends the process; `_tailer` lives until then.
    let stop = AtomicBool::new(false);
    let mut role = crate::contract::Role::Unspecified;
    log.subscribe(from, &stop, |ev| {
        render_watch_event(ev, &mut role);
        true
    })
}

/// Render one event to the terminal for `session watch` — a readable view of
/// the agent's stream (messages by role, tools, thinking, the attention
/// signal), not raw JSON. Ephemeral telemetry (usage, lifecycle) is skipped.
fn render_watch_event(ev: &crate::contract::Event, role: &mut crate::contract::Role) {
    use crate::contract::{Payload, Role, ToolStatus};
    match &ev.payload {
        Payload::MessageStart(m) => *role = m.role,
        Payload::MessageDelta(d) => {
            let who = match *role {
                Role::User => "you",
                Role::Assistant => "claude",
                Role::System => "system",
                Role::Unspecified => "agent",
            };
            println!("{who}: {}", d.text.trim_end());
        }
        Payload::ToolCall(t) if t.status == ToolStatus::Running => {
            let arg = t.input.as_ref().map(summarize_one_line).unwrap_or_default();
            println!("  ⚙ {} {arg}", t.name);
        }
        Payload::ToolCall(t) => {
            let mark = if t.status == ToolStatus::Error {
                "✗"
            } else {
                "✓"
            };
            let out = first_line(&t.output);
            println!(
                "  {mark} {}",
                if out.is_empty() { "(done)".into() } else { out }
            );
        }
        Payload::Thinking(th) => println!("  · {}", first_line(&th.text)),
        Payload::AttentionRequired(a) => println!("⏳ needs attention ({:?})", a.reason),
        _ => {} // usage / lifecycle / unknown — not rendered
    }
}

/// First non-empty line of `s`, char-safely capped for a terminal line.
fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let capped: String = line.chars().take(100).collect();
    if line.chars().count() > 100 {
        format!("{capped}…")
    } else {
        capped
    }
}

/// A compact one-line view of a tool's JSON input.
fn summarize_one_line(v: &serde_json::Value) -> String {
    first_line(&v.to_string())
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
