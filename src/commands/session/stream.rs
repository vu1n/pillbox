//! The live-read / streaming surface of `session` — resolving a session for
//! streaming (spawning the transcript/event drain), serving it over WebSocket
//! (`subscribe`), rendering it to the terminal (`watch`), and blocking until a
//! turn goes idle (`wait-idle`). Split out of `mod.rs` to keep the lifecycle
//! commands separate from the read plane. Parent-private helpers (`opencode_http`,
//! the libkrun cfg accessors) are reached via `super::`.

use anyhow::Result;

use crate::agents::Integration;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{events, sandbox, session};

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
) -> Result<(String, Option<events::transcripts::TailerHandle>)> {
    if let Ok(s) = session::resolve(resolved, id) {
        // Server-integration agents (opencode) have no transcript file — read
        // their HTTP `/event` stream into the log via the bridge instead.
        if s.integration() == Integration::Server {
            let log = crate::events::log::SessionLog::open(resolved, &s.id)?;
            // libkrun captures /event to a persistent file in the shared home, so
            // drain THAT (replay + follow) — complete even for a late watcher, no
            // host daemon. Docker reads the live /event bridge, which only captures
            // while watched.
            let tailer = match session::Backend::parse(&s.backend) {
                // A detached §0 producer already keeps the log live — just follow it (a second
                // drainer would double-write). Else this reader is the drainer (no producer).
                Some(session::Backend::Libkrun) if super::detached_tailer_alive(resolved, &s) => {
                    None
                }
                Some(session::Backend::Libkrun) => super::libkrun_server_file_tailer(&s, log),
                _ => match super::server_http(&s) {
                    Ok(http) => sandbox::opencode::spawn_event_bridge(&*http, &s.id, log),
                    Err(e) => {
                        eprintln!(
                            "pillbox: note: can't reach the opencode server ({e}); \
                             reading the existing log"
                        );
                        None
                    }
                },
            };
            return Ok((s.id, tailer));
        }
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
            // A libkrun PTY session's transcript tailing isn't wired here yet;
            // read the existing host log.
            _ => {
                eprintln!(
                    "pillbox: note: live event tailing isn't available for `{}` sessions; \
                     reading the existing log",
                    s.backend
                );
                None
            }
        };
        return Ok((s.id, tailer));
    }
    // Foreground/historical run: a durable LOG but no `.toml` record.
    Ok((session::resolve_logged(resolved, id)?, None))
}

pub(super) fn session_subscribe(
    resolved: &Pillbox,
    id: &str,
    from: u64,
    bind: Option<&str>,
) -> Result<()> {
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

pub(super) fn session_watch(resolved: &Pillbox, id: &str, from: u64) -> Result<()> {
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
                Role::Assistant => "assistant",
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

/// Block until the session's current turn goes idle — the drive-surface "turn
/// done" primitive (an orchestrator `send`s a prompt, then `wait-idle` instead of
/// polling). Spawns the same live drain `watch`/`subscribe` use, so the turn's
/// events — including the idle signal — land in the durable log as the agent
/// works (the trajectory is then already persisted; a later `session ingest` is
/// redundant). "Idle" = the §0 `AttentionRequired` signal (the agent yielded for
/// input) or a terminal `RunFinished`/`RunFailed`. Returns Err (exit 1) on timeout.
pub(super) fn session_wait_idle(
    resolved: &Pillbox,
    id: &str,
    timeout: Option<u64>,
    from: Option<u64>,
) -> Result<()> {
    use crate::contract::Payload;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Capture the log tail BEFORE the drain spawns, so the default "wait for the
    // next idle" can't race the tailer: events it appends get seq > baseline, and
    // subscribing from baseline+1 catches them (computing last_seq after spawn
    // could skip an already-drained idle). Default = next idle, not a stale one.
    let s = session::resolve(resolved, id)?;
    let baseline = crate::events::log::SessionLog::open(resolved, &s.id)?.last_seq();
    let from = from.unwrap_or(baseline + 1);

    // The tailer drains the §0 capture into the log while we wait; it stops when
    // `_tailer` drops at fn return (TailerHandle's Drop joins it).
    let (sid, _tailer) = resolve_streaming_session(resolved, id, "session wait-idle")?;
    let log = crate::events::log::SessionLog::open(resolved, &sid)?;

    let stop = Arc::new(AtomicBool::new(false));
    if let Some(secs) = timeout {
        // Watchdog: flip `stop` after the deadline, which ends `subscribe`'s wait.
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            stop.store(true, Ordering::Relaxed);
        });
    }

    let mut idle = false;
    log.subscribe(from, &stop, |ev| {
        let done = matches!(
            ev.payload,
            Payload::AttentionRequired(_) | Payload::RunFinished(_) | Payload::RunFailed(_)
        );
        if done {
            idle = true;
        }
        !done // keep subscribing until a turn-done event (or `stop`/timeout)
    })?;

    if idle {
        println!("pillbox: session `{sid}` idle");
        Ok(())
    } else {
        Err(PillboxError::runtime(
            "session wait-idle",
            format!("session `{sid}` did not go idle within the timeout"),
        )
        .into())
    }
}
