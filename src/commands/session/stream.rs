//! The live-read / streaming surface of `session` — resolving a session for
//! streaming (spawning the transcript/event drain), serving it over WebSocket
//! (`subscribe`), rendering it to the terminal (`watch`), and blocking until a
//! turn goes idle (`wait-idle`). Split out of `mod.rs` to keep the lifecycle
//! commands separate from the read plane. The live source + its tailer come from
//! the [`LiveSession`](crate::sandbox::LiveSession) plane, so this file does not
//! branch on the backend.

use anyhow::Result;

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{events, sandbox, session};

/// Resolve a session id/prefix for streaming and ensure its log is being
/// filled. A live record → open the backend's §0 source + the tailer that fills
/// it (the tailer is returned as a guard the caller holds for the stream's
/// lifetime, so a `session send`-driven session is readable as it runs); a
/// backend that can't host-tail (or whose source open fails) → note it and read
/// the existing log; a foreground/historical run → resolve the log dir. Shared
/// by `session subscribe` (serves WS) and `session watch` (renders to terminal).
fn resolve_streaming_session(
    resolved: &Pillbox,
    id: &str,
) -> Result<(String, Option<events::transcripts::TailerHandle>)> {
    if let Ok(s) = session::resolve(resolved, id) {
        // Gate on the *capability*, not on catching every error: a backend that
        // can host-tail this session (docker any session, libkrun a server one)
        // spawns the tailer and lets a genuine failure (registry miss, IO)
        // propagate loud; one that can't (a libkrun PTY session, an
        // unknown/removed backend) degrades to reading the existing log. Catching
        // every `event_source` error instead would silently swallow a real failure
        // as "no live tail" — and break `wait-idle`. The source is reopened
        // per-caller below, so only the tailer guard is kept here.
        let tailer = match sandbox::live_session(&s) {
            Ok(live) => {
                let can_tail = if s.integration() == crate::agents::Integration::Server {
                    live.caps().server_mode
                } else {
                    live.caps().live_pty_tail
                };
                if can_tail {
                    live.spawn_log_tailer(resolved)?
                } else {
                    eprintln!(
                        "pillbox: note: live event tailing isn't available for this `{}` \
                         session; reading the existing log",
                        s.backend
                    );
                    None
                }
            }
            Err(_) => {
                eprintln!(
                    "pillbox: note: this binary can't reach the `{}` backend; \
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
    let (sid, _tailer) = resolve_streaming_session(resolved, id)?;
    // Read-side fan-out: while the log is being filled (live session), if a
    // notification webhook is configured, tail the log and POST attention
    // signals to it — a consumer of the log (its own read view), off the
    // tailer's producer path.
    if _tailer.is_some() {
        if let Some(url) = std::env::var("PILLBOX_EVENTS_WEBHOOK")
            .ok()
            .filter(|u| !u.is_empty())
        {
            // NOTE: local-only — the webhook exporter tails the local file log,
            // not the managed DO source. Under managed placement it would see an
            // empty local log; migrating it (and `wait-idle`) to open_event_source
            // needs a last_seq on the EventSource trait. Deferred follow-up.
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
    let (sid, _tailer) = resolve_streaming_session(resolved, id)?;
    eprintln!("pillbox: watching session {sid} (Ctrl-C to stop)");
    // Read through the placement swap point: the local file by default, the
    // managed DO WebSocket when the managed tier is on.
    let source = crate::events::source::open_event_source(resolved, &sid)?;
    // Never set: Ctrl-C ends the process; `_tailer` lives until then.
    let stop = AtomicBool::new(false);
    let mut role = crate::contract::Role::Unspecified;
    source.subscribe(from, &stop, &mut |ev| {
        render_watch_event(ev, &mut role);
        true
    })
}

/// Render one event to the terminal for `session watch`. Pure formatting lives
/// in [`format_watch_event`] (testable); this just tracks role and prints.
fn render_watch_event(ev: &crate::contract::Event, role: &mut crate::contract::Role) {
    if let crate::contract::Payload::MessageStart(m) = &ev.payload {
        *role = m.role;
    }
    if let Some(line) = format_watch_event(ev, *role) {
        println!("{line}");
    }
}

/// Format one event into a readable line for `session watch` — a human view of
/// the agent's stream (messages by role, tools, thinking, the attention signal,
/// the multiplayer steer/chime-in), not raw JSON. Returns `None` for ephemeral
/// telemetry (usage, lifecycle) that the watcher skips. `role` is the running
/// message role from the latest `MessageStart`.
fn format_watch_event(ev: &crate::contract::Event, role: crate::contract::Role) -> Option<String> {
    use crate::contract::{Payload, Role, ToolStatus};
    // Attribution tag: who produced this event. `[u:alice]`-style (the id is
    // already kind-prefixed); empty for legacy/unattributed events.
    let tag = ev
        .actor
        .as_ref()
        .map(|a| format!("[{}] ", a.id))
        .unwrap_or_default();
    Some(match &ev.payload {
        Payload::MessageStart(_) => return None,
        Payload::MessageDelta(d) => {
            let who = match role {
                Role::User => "you",
                Role::Assistant => "assistant",
                Role::System => "system",
                Role::Unspecified => "agent",
            };
            format!("{tag}{who}: {}", d.text.trim_end())
        }
        Payload::ToolCall(t) if t.status == ToolStatus::Running => {
            let arg = t.input.as_ref().map(summarize_one_line).unwrap_or_default();
            format!("  ⚙ {tag}{} {arg}", t.name)
        }
        Payload::ToolCall(t) => {
            let mark = if t.status == ToolStatus::Error {
                "✗"
            } else {
                "✓"
            };
            let out = first_line(&t.output);
            format!(
                "  {mark} {tag}{}",
                if out.is_empty() { "(done)".into() } else { out }
            )
        }
        Payload::Thinking(th) => format!("  · {tag}{}", first_line(&th.text)),
        Payload::AttentionRequired(a) => format!("⏳ {tag}needs attention ({:?})", a.reason),
        // The durable steer (who drove the agent, with what).
        Payload::Input(i) => format!("▶ {tag}drove: {}", first_line(&i.text)),
        // The non-driving chime-in; show its anchor when it references something.
        Payload::Annotation(an) => {
            let anchor = if an.anchor.is_empty() {
                String::new()
            } else {
                format!(" @{}", an.anchor)
            };
            format!("✎ {tag}noted{anchor}: {}", first_line(&an.text))
        }
        _ => return None, // usage / lifecycle / unknown — not rendered
    })
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
    let (sid, _tailer) = resolve_streaming_session(resolved, id)?;
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

#[cfg(test)]
mod tests {
    use super::format_watch_event;
    use crate::contract::{
        Actor, Annotation, Event, Input, InputTarget, MessageDelta, Payload, Role,
    };

    fn msg(text: &str) -> Payload {
        Payload::MessageDelta(MessageDelta {
            message_id: "m".into(),
            text: text.into(),
        })
    }

    #[test]
    fn actor_renders_as_compact_tag() {
        let ev = Event::session("s", msg("hi")).with_actor(Actor::agent("claude"));
        let line = format_watch_event(&ev, Role::Assistant).unwrap();
        assert_eq!(line, "[a:claude] assistant: hi");
    }

    #[test]
    fn missing_actor_renders_no_tag() {
        let ev = Event::session("s", msg("hi"));
        let line = format_watch_event(&ev, Role::Assistant).unwrap();
        assert_eq!(line, "assistant: hi");
    }

    #[test]
    fn input_renders_driver_and_text() {
        let ev = Event::session(
            "s",
            Payload::Input(Input {
                text: "run the tests".into(),
                target: InputTarget::Agent,
            }),
        )
        .with_actor(Actor::human("alice"));
        let line = format_watch_event(&ev, Role::Unspecified).unwrap();
        assert_eq!(line, "▶ [u:alice] drove: run the tests");
    }

    #[test]
    fn annotation_renders_author_anchor_and_text() {
        let ev = Event::session(
            "s",
            Payload::Annotation(Annotation {
                text: "watch the retry path".into(),
                anchor: "src/net.rs".into(),
            }),
        )
        .with_actor(Actor::human("bob"));
        let line = format_watch_event(&ev, Role::Unspecified).unwrap();
        assert_eq!(line, "✎ [u:bob] noted @src/net.rs: watch the retry path");
    }
}
