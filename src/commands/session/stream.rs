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
            // The webhook exporter tails the local file log. Managed execution
            // copies its bounded evidence into that same log before returning.
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
    // All placements expose their evidence through the local session log.
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

/// A §0 circuit-breaker: subscribe to a session's LIVE event stream, run a pure
/// pathology detector over each event, and on a trip either kill the session
/// (`kill`) or just log it (the default dry-run). Modeled on the
/// [`spawn_webhook_log_exporter`](crate::events::spawn_webhook_log_exporter)
/// precedent — an external consumer that live-tails §0 and acts — but the action
/// is "kill" (or log) instead of "POST", and it runs in the foreground (the sink
/// returns `false` once tripped, ending the subscribe loop).
///
/// A standalone monitor: it does NOT touch `dispatch`'s synchronous
/// send→wait-idle loop (wiring it in is a deliberate follow-up). The kill reuses
/// `session rm`'s exact path (`live_session(&s).kill`) — orphan-safe (reap-by-
/// unique-spec), so no host-wide `~/.pillbox/krun` sweep. A resolve-miss (the
/// session was already removed) is tolerated: log + exit 0.
pub(super) fn session_guard(
    resolved: &Pillbox,
    id: &str,
    max_repeats: u32,
    max_tokens: u64,
    kill: bool,
) -> Result<()> {
    use std::sync::atomic::AtomicBool;

    // Spawn the live drain (same as `wait-idle`) so the log fills while we watch
    // a `send`-driven/headless session — the tailer guard lives until fn return.
    let (sid, _tailer) = resolve_streaming_session(resolved, id)?;
    eprintln!(
        "pillbox: guarding session {sid} (max-repeats={max_repeats}, max-tokens={max_tokens}, \
         {}); Ctrl-C to stop",
        if kill { "armed: --kill" } else { "dry-run" }
    );
    // Subscribe to the local session log from the current head — a breaker reacts
    // to NEW pathology, not a past one (mirrors the webhook exporter's
    // `last_seq + 1`).
    let from = crate::events::log::SessionLog::open(resolved, &sid)?.last_seq() + 1;
    let source = crate::events::source::open_event_source(resolved, &sid)?;

    let mut detector = PathologyDetector::new(max_repeats, max_tokens);
    let mut tripped: Option<String> = None;
    // Never set in-process: Ctrl-C ends the process; the sink ends the loop on a
    // trip by returning `false`. `_tailer` lives until then.
    let stop = AtomicBool::new(false);
    source.subscribe(from, &stop, &mut |ev| match detector.observe(ev) {
        Some(reason) => {
            tripped = Some(reason);
            false // stop subscribing — we've seen the pathology
        }
        None => true,
    })?;

    let reason = match tripped {
        Some(r) => r,
        None => {
            // The local stream ended without a trip.
            eprintln!("pillbox: guard on session `{sid}` ended without tripping");
            return Ok(());
        }
    };
    eprintln!("pillbox: ⚠ guard tripped on session `{sid}`: {reason}");
    if !kill {
        println!("pillbox: would kill session `{sid}` ({reason}) — re-run with --kill to arm");
        return Ok(());
    }
    // Arm the teardown: reuse `session rm`'s orphan-safe kill exactly. Resolve
    // first (the breaker held only the streaming sid); a resolve-miss means the
    // session is already gone — log + exit 0 rather than erroring.
    let Ok(s) = session::resolve(resolved, &sid) else {
        eprintln!("pillbox: session `{sid}` already removed; nothing to kill");
        return Ok(());
    };
    sandbox::live_session(&s)?.kill(resolved)?;
    println!("pillbox: ✓ guard killed session `{sid}` ({reason})");
    Ok(())
}

/// A PURE pathology detector over a session's §0 event stream — no I/O, so it's
/// unit-tested over a `&[Event]` (the `select_winner`/`distill_feedback` pure-
/// policy pattern). `observe` folds one event into the running state and returns
/// `Some(reason)` the first time a signal trips. Three signals:
///   1. **Repeated identical tool call** — consecutive `ToolStatus::Running`
///      with the same (name, input) key, `>= max_repeats` times (when > 0).
///   2. **Error spiral** — `>= max_repeats` consecutive `ToolStatus::Error`
///      (same threshold), and any `RunFailed` is an immediate trip.
///   3. **Token blowout** — cumulative `Usage` input+output tokens past
///      `max_tokens` (when > 0).
///
/// With both thresholds 0, only `RunFailed` trips.
struct PathologyDetector {
    /// Consecutive-identical-ToolCall threshold AND consecutive-error threshold;
    /// 0 disables both (RunFailed still trips).
    max_repeats: u32,
    /// Cumulative input+output token budget; 0 disables the blowout detector.
    max_tokens: u64,
    /// The (name, input) key of the last `Running` tool call + its run length.
    last_call: Option<(String, String)>,
    repeat_run: u32,
    /// Consecutive `ToolStatus::Error` count.
    error_run: u32,
    /// Running sum of `Usage` input+output tokens.
    tokens: u64,
}

impl PathologyDetector {
    fn new(max_repeats: u32, max_tokens: u64) -> Self {
        Self {
            max_repeats,
            max_tokens,
            last_call: None,
            repeat_run: 0,
            error_run: 0,
            tokens: 0,
        }
    }

    /// Fold one event into the detector; `Some(reason)` on the first trip.
    fn observe(&mut self, ev: &crate::contract::Event) -> Option<String> {
        use crate::contract::{Payload, ToolStatus};
        match &ev.payload {
            // A run-level failure is always an immediate trip.
            Payload::RunFailed(f) => {
                return Some(format!("run failed: {}", f.reason));
            }
            Payload::ToolCall(t) => match t.status {
                ToolStatus::Running => {
                    // A non-error step breaks an error spiral.
                    self.error_run = 0;
                    // Key on (name, input): the same call with the same args,
                    // back to back, is the spin. `input` is canonicalized to a
                    // string so equality is structural, not pointer.
                    let key = (
                        t.name.clone(),
                        t.input
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                    );
                    if self.last_call.as_ref() == Some(&key) {
                        self.repeat_run += 1;
                    } else {
                        self.last_call = Some(key);
                        self.repeat_run = 1;
                    }
                    if self.max_repeats > 0 && self.repeat_run >= self.max_repeats {
                        let (name, _) = self.last_call.as_ref().expect("set above");
                        return Some(format!(
                            "repeated tool call `{name}` {}× in a row",
                            self.repeat_run
                        ));
                    }
                }
                ToolStatus::Error => {
                    self.error_run += 1;
                    if self.max_repeats > 0 && self.error_run >= self.max_repeats {
                        return Some(format!(
                            "{}× consecutive tool errors (error spiral)",
                            self.error_run
                        ));
                    }
                }
                // A completed/other tool status breaks the error spiral; it
                // isn't a `Running` start, so the repeat run is untouched.
                ToolStatus::Completed | ToolStatus::Unspecified => {
                    self.error_run = 0;
                }
            },
            Payload::Usage(u) => {
                self.tokens += u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0);
                if self.max_tokens > 0 && self.tokens > self.max_tokens {
                    return Some(format!(
                        "token blowout: {} tokens > budget {}",
                        self.tokens, self.max_tokens
                    ));
                }
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{format_watch_event, PathologyDetector};
    use crate::contract::{
        Actor, Annotation, Event, Input, InputTarget, MessageDelta, Payload, Role, RunFailed,
        ToolCall, ToolStatus, Usage, UsageSource,
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

    // ── PathologyDetector (pure; no VM) ──────────────────────────────────────

    /// A `Running` tool call with `name` + JSON `input`.
    fn running(name: &str, input: serde_json::Value) -> Event {
        Event::session(
            "s",
            Payload::ToolCall(ToolCall {
                tool_call_id: format!("tc-{name}"),
                name: name.into(),
                status: ToolStatus::Running,
                input: Some(input),
                output: String::new(),
                title: String::new(),
            }),
        )
    }

    /// A tool call with an explicit status (for error-spiral / completed cases).
    fn tool_status(name: &str, status: ToolStatus) -> Event {
        Event::session(
            "s",
            Payload::ToolCall(ToolCall {
                tool_call_id: format!("tc-{name}"),
                name: name.into(),
                status,
                input: None,
                output: String::new(),
                title: String::new(),
            }),
        )
    }

    /// A `Usage` event carrying `input`/`output` token counts.
    fn usage(input: u64, output: u64) -> Event {
        Event::session(
            "s",
            Payload::Usage(Usage {
                message_id: "m".into(),
                input_tokens: Some(input),
                output_tokens: Some(output),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
                source: UsageSource::Wire,
            }),
        )
    }

    /// Fold a slice through the detector, returning the first trip reason (pure).
    fn first_trip(d: &mut PathologyDetector, events: &[Event]) -> Option<String> {
        events.iter().find_map(|ev| d.observe(ev))
    }

    #[test]
    fn trips_after_n_identical_running_tool_calls() {
        // The same (name, input) call three times in a row trips at max_repeats=3.
        let evs = [
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Bash", serde_json::json!({"command": "ls"})),
        ];
        let mut d = PathologyDetector::new(3, 0);
        let trip = first_trip(&mut d, &evs).expect("3 identical calls must trip");
        assert!(trip.contains("repeated tool call"), "{trip}");
        assert!(trip.contains("Bash"), "{trip}");
    }

    #[test]
    fn does_not_trip_on_distinct_or_interleaved_calls() {
        // N distinct calls (different inputs) never reach a run of max_repeats.
        let distinct = [
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Bash", serde_json::json!({"command": "pwd"})),
            running("Bash", serde_json::json!({"command": "cat x"})),
        ];
        let mut d = PathologyDetector::new(3, 0);
        assert!(
            first_trip(&mut d, &distinct).is_none(),
            "distinct inputs must not trip"
        );

        // Healthy interleaving: same call, but a different call breaks each run.
        let interleaved = [
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Read", serde_json::json!({"path": "a"})),
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Read", serde_json::json!({"path": "a"})),
        ];
        let mut d = PathologyDetector::new(2, 0);
        assert!(
            first_trip(&mut d, &interleaved).is_none(),
            "interleaved calls must not trip"
        );
    }

    #[test]
    fn trips_on_consecutive_errors_and_immediately_on_run_failed() {
        // Two consecutive tool errors trip at max_repeats=2.
        let errs = [
            tool_status("Bash", ToolStatus::Error),
            tool_status("Bash", ToolStatus::Error),
        ];
        let mut d = PathologyDetector::new(2, 0);
        let trip = first_trip(&mut d, &errs).expect("error spiral must trip");
        assert!(trip.contains("consecutive tool errors"), "{trip}");

        // RunFailed trips immediately, regardless of thresholds (both 0 here).
        let failed = [Event::session(
            "s",
            Payload::RunFailed(RunFailed {
                reason: "boom".into(),
                exit_code: 1,
            }),
        )];
        let mut d = PathologyDetector::new(0, 0);
        let trip = first_trip(&mut d, &failed).expect("RunFailed must always trip");
        assert!(trip.contains("run failed"), "{trip}");
        assert!(trip.contains("boom"), "{trip}");
    }

    #[test]
    fn trips_when_cumulative_tokens_exceed_budget() {
        // Cumulative input+output crosses the budget on the second Usage.
        let evs = [usage(40, 30), usage(20, 20)]; // 70, then 110
        let mut d = PathologyDetector::new(0, 100);
        let trip = first_trip(&mut d, &evs).expect("110 > 100 must trip");
        assert!(trip.contains("token blowout"), "{trip}");

        // Below budget: no trip.
        let mut d = PathologyDetector::new(0, 100);
        assert!(
            first_trip(&mut d, &[usage(40, 30)]).is_none(),
            "70 <= 100 must not trip"
        );
    }

    #[test]
    fn a_differing_event_resets_the_run() {
        // Two identical calls, a different call, then two more identical: no run
        // ever reaches three, so max_repeats=3 must not trip.
        let evs = [
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Read", serde_json::json!({"path": "x"})),
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Bash", serde_json::json!({"command": "ls"})),
        ];
        let mut d = PathologyDetector::new(3, 0);
        assert!(
            first_trip(&mut d, &evs).is_none(),
            "a differing call must reset the repeat run"
        );

        // Likewise a non-error (Completed) step breaks an error spiral.
        let evs = [
            tool_status("Bash", ToolStatus::Error),
            tool_status("Bash", ToolStatus::Completed),
            tool_status("Bash", ToolStatus::Error),
        ];
        let mut d = PathologyDetector::new(2, 0);
        assert!(
            first_trip(&mut d, &evs).is_none(),
            "a completed step must reset the error run"
        );
    }

    #[test]
    fn no_flags_trips_only_on_run_failed() {
        // With both thresholds 0, repeats and errors are inert — only RunFailed.
        let benign = [
            running("Bash", serde_json::json!({"command": "ls"})),
            running("Bash", serde_json::json!({"command": "ls"})),
            tool_status("Bash", ToolStatus::Error),
            tool_status("Bash", ToolStatus::Error),
            usage(10_000, 10_000),
        ];
        let mut d = PathologyDetector::new(0, 0);
        assert!(
            first_trip(&mut d, &benign).is_none(),
            "no flags → inert on repeats/errors/tokens"
        );
    }
}
