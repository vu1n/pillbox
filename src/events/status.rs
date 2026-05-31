//! Derive a session's *status* by folding the two host-visible signal streams.
//!
//! A session's "what is it doing right now" isn't stored on the record — it's
//! latent in the events it has emitted. Two streams carry it, and this module
//! is the single place that folds them so `session list`, `session info`, and
//! `session diagnose` agree:
//!
//!   - the **shared lifecycle sink** (`<pillbox>/events.jsonl`) — terminal
//!     `session.completed` / `session.failed`, keyed by `session_id`. Folded
//!     once via [`terminal_outcomes`] so a `list` over N sessions is one read,
//!     not N.
//!   - the **per-session durable log** (`sessions/<id>/log.jsonl`) — the
//!     `end_turn`→`NeedsInput` attention signal and message/tool activity.
//!
//! **Honesty about reach.** Only host-visible signals count. A remote/detached
//! session emits its terminal event sandbox-side, so until that reaches the
//! host (a webhook listener replaying `session done`, or `session pull`
//! persisting `result_snapshot`) the host genuinely can't know it finished —
//! such a session reads `Running` (it launched), not a guessed `Done`. The
//! per-session log likewise only exists host-side for live-tailed local runs.
//! The deriver never overclaims past what the host can see.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::contract::{AttentionReason, Payload, Role, ToolStatus};
use crate::events::{events_path, log};
use crate::pillbox::Pillbox;
use crate::session::Session;

/// What a session is doing, in precedence order (terminal wins). There's no
/// distinct "starting" — a session only has a record once it's launched, so the
/// resting non-terminal state is `Running` (a remote session the host can't see
/// into reads `Running` until its terminal event arrives — honest, not a
/// guessed "done").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionStatus {
    /// Producing output / running tools (or launched-but-host-opaque).
    Running,
    /// The agent ended a turn awaiting input (`end_turn`→`NeedsInput`) and
    /// hasn't resumed — the front-end's cue to flash / seek input.
    NeedsInput,
    /// Finished successfully (host saw `session.completed`, or a result
    /// snapshot was persisted to the record).
    Done,
    /// Finished with an error (host saw `session.failed`).
    Failed,
}

impl SessionStatus {
    /// Stable lower-kebab label for the CLI column and the `--json` field.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::NeedsInput => "needs-input",
            SessionStatus::Done => "done",
            SessionStatus::Failed => "failed",
        }
    }
}

/// One session's folded view: its [`SessionStatus`] plus the activity counts
/// `session diagnose` renders. Produced by a single pass over the durable log
/// ([`summarize`]) so status and counts can never disagree about what a "turn"
/// or "tool call" is — the classification lives in exactly one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Diagnosis {
    pub(crate) status: SessionStatus,
    pub(crate) assistant_turns: u64,
    pub(crate) tool_calls: u64,
    pub(crate) last_at: String,
    pub(crate) log_seq: u64,
}

/// A host-visible terminal outcome for a session, with the detail `diagnose`
/// surfaces (the same single parse serves both `list` status and the readout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Terminal {
    Done {
        exit_code: Option<i64>,
    },
    Failed {
        reason: String,
        exit_code: Option<i64>,
    },
}

/// Only the fields of an `events.jsonl` line we need. Everything else is
/// ignored, so unknown/added fields don't break the read.
#[derive(Deserialize)]
struct LifecycleLine {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    exit_code: Option<i64>,
}

/// Fold the shared lifecycle sink once into the latest terminal outcome per
/// session id. Empty when `events.jsonl` doesn't exist. A later terminal line
/// overwrites an earlier one (the last word for a session wins).
pub(crate) fn terminal_outcomes(pb: &Pillbox) -> Result<HashMap<String, Terminal>> {
    let path = events_path(pb);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut out = HashMap::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Tolerate a malformed line rather than failing the whole list — the
        // sink is append-only JSONL a crash could truncate mid-write.
        let Ok(parsed) = serde_json::from_str::<LifecycleLine>(line) else {
            continue;
        };
        let outcome = match parsed.event.as_str() {
            "session.completed" => Terminal::Done {
                exit_code: parsed.exit_code,
            },
            "session.failed" => Terminal::Failed {
                reason: parsed.reason.unwrap_or_default(),
                exit_code: parsed.exit_code,
            },
            _ => continue,
        };
        if !parsed.session_id.is_empty() {
            out.insert(parsed.session_id, outcome);
        }
    }
    Ok(out)
}

/// Fold a session in one read-only pass over its durable log (never creates the
/// session dir): activity counts + whether its last turn left the agent
/// awaiting input, combined with its `terminal` outcome (looked up from
/// [`terminal_outcomes`], or `None`) into a [`Diagnosis`]. The single classifier
/// `list` (status only), `info`, and `diagnose` (full counts) all read.
pub(crate) fn summarize(
    pb: &Pillbox,
    session: &Session,
    terminal: Option<&Terminal>,
) -> Result<Diagnosis> {
    // `pending_input` tracks the last turn's resting state: set by the
    // `end_turn`→NeedsInput signal, cleared the moment the agent resumes. A
    // `MessageStart` always precedes the `Delta`/`End` of its turn, so it alone
    // covers "the agent is producing again" — Delta/End add nothing.
    let mut pending_input = false;
    let mut assistant_turns = 0;
    let mut tool_calls = 0;
    let mut last_at = String::new();
    let mut log_seq = 0;
    for ev in log::read_log(pb, &session.id)? {
        log_seq = ev.seq;
        if !ev.at.is_empty() {
            last_at = ev.at;
        }
        match ev.payload {
            Payload::AttentionRequired(a) if a.reason == AttentionReason::NeedsInput => {
                pending_input = true;
            }
            Payload::MessageStart(m) => {
                pending_input = false;
                if m.role == Role::Assistant {
                    assistant_turns += 1;
                }
            }
            // A tool call lands twice (Running, then its correlated result);
            // count the Running side so the number is invocations, not events.
            Payload::ToolCall(t) if t.status == ToolStatus::Running => {
                pending_input = false;
                tool_calls += 1;
            }
            Payload::Thinking(_) => pending_input = false,
            _ => {}
        }
    }

    // Precedence: a host-visible terminal (or persisted result snapshot — the
    // agent finished + pushed its result host-side) wins; else the log's last
    // turn decides needs-input vs running.
    let status = match terminal {
        Some(Terminal::Failed { .. }) => SessionStatus::Failed,
        Some(Terminal::Done { .. }) => SessionStatus::Done,
        None if session.result_snapshot.is_some() => SessionStatus::Done,
        None if pending_input => SessionStatus::NeedsInput,
        None => SessionStatus::Running,
    };
    Ok(Diagnosis {
        status,
        assistant_turns,
        tool_calls,
        last_at,
        log_seq,
    })
}

/// Status-only view for `list`/`info` — [`summarize`] then the status.
pub(crate) fn derive(
    pb: &Pillbox,
    session: &Session,
    terminal: Option<&Terminal>,
) -> Result<SessionStatus> {
    Ok(summarize(pb, session, terminal)?.status)
}

/// Single-session convenience (`info`): fold the sink for just this id.
pub(crate) fn derive_one(pb: &Pillbox, session: &Session) -> Result<SessionStatus> {
    let terminal = terminal_outcomes(pb)?;
    derive(pb, session, terminal.get(&session.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{AttentionRequired, Event, MessageStart, Role};
    use crate::events::log::SessionLog;
    use crate::test_util::with_isolated_home;

    fn msg(role: Role) -> Payload {
        Payload::MessageStart(MessageStart {
            message_id: "m".into(),
            role,
        })
    }

    fn needs_input() -> Payload {
        Payload::AttentionRequired(AttentionRequired {
            reason: AttentionReason::NeedsInput,
            message: String::new(),
        })
    }

    fn tool(status: ToolStatus) -> Payload {
        Payload::ToolCall(crate::contract::ToolCall {
            tool_call_id: "tc".into(),
            name: "Bash".into(),
            status,
            input: None,
            output: String::new(),
            title: String::new(),
        })
    }
    fn tool_running() -> Payload {
        tool(ToolStatus::Running)
    }
    fn tool_done() -> Payload {
        tool(ToolStatus::Completed)
    }

    fn sess(id: &str) -> Session {
        let mut s = Session::test_fixture();
        s.id = id.into();
        s.attached_pid = None;
        s.result_snapshot = None;
        s
    }

    #[test]
    fn terminal_outcome_wins_over_everything() {
        with_isolated_home("status-terminal", || {
            let pb = crate::pillbox::global();
            let s = sess("aaaa11112222");
            let failed = Terminal::Failed {
                reason: "boom".into(),
                exit_code: Some(1),
            };
            let done = Terminal::Done { exit_code: Some(0) };
            assert_eq!(
                derive(&pb, &s, Some(&failed)).unwrap(),
                SessionStatus::Failed
            );
            assert_eq!(derive(&pb, &s, Some(&done)).unwrap(), SessionStatus::Done);
        });
    }

    #[test]
    fn persisted_result_snapshot_means_done() {
        with_isolated_home("status-resultsnap", || {
            let pb = crate::pillbox::global();
            let mut s = sess("bbbb11112222");
            s.result_snapshot = Some("snap".into());
            assert_eq!(derive(&pb, &s, None).unwrap(), SessionStatus::Done);
        });
    }

    #[test]
    fn attention_is_sticky_until_the_agent_resumes() {
        with_isolated_home("status-needsinput", || {
            let pb = crate::pillbox::global();
            let s = sess("cccc11112222");
            let mut log = SessionLog::open(&pb, &s.id).unwrap();
            // A turn that ends awaiting input (attention lands after the message).
            log.append(&[
                Event::session(&s.id, msg(Role::Assistant)),
                Event::session(&s.id, needs_input()),
            ])
            .unwrap();
            assert_eq!(derive(&pb, &s, None).unwrap(), SessionStatus::NeedsInput);
            // The user's next message clears it → back to running.
            log.append(&[Event::session(&s.id, msg(Role::User))])
                .unwrap();
            assert_eq!(derive(&pb, &s, None).unwrap(), SessionStatus::Running);
        });
    }

    #[test]
    fn a_recorded_session_with_no_host_signal_is_running() {
        // No log activity, no terminal — e.g. a remote session the host can't
        // see into. It launched (it has a record), so the honest resting state
        // is `Running`, not a guessed "done" or a stuck "starting".
        with_isolated_home("status-running-default", || {
            let pb = crate::pillbox::global();
            let s = sess("dddd11112222");
            assert_eq!(derive(&pb, &s, None).unwrap(), SessionStatus::Running);
        });
    }

    #[test]
    fn summarize_counts_turns_and_tool_calls_in_one_pass() {
        with_isolated_home("status-counts", || {
            let pb = crate::pillbox::global();
            let s = sess("eeee11112222");
            let mut log = SessionLog::open(&pb, &s.id).unwrap();
            log.append(&[
                Event::session(&s.id, msg(Role::Assistant)),
                Event::session(&s.id, tool_running()),
                Event::session(&s.id, tool_done()), // the result half — not double-counted
                Event::session(&s.id, msg(Role::Assistant)),
            ])
            .unwrap();
            let d = summarize(&pb, &s, None).unwrap();
            assert_eq!(d.assistant_turns, 2);
            assert_eq!(
                d.tool_calls, 1,
                "the Running half counts, the result doesn't"
            );
            assert_eq!(d.log_seq, 4);
            assert_eq!(d.status, SessionStatus::Running);
        });
    }

    #[test]
    fn terminal_outcomes_keeps_the_last_terminal_per_session() {
        with_isolated_home("status-outcomes", || {
            let pb = crate::pillbox::global();
            let path = events_path(&pb);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                concat!(
                    "{\"event\":\"session.started\",\"session_id\":\"s1\"}\n",
                    "{\"event\":\"session.completed\",\"session_id\":\"s1\",\"exit_code\":0}\n",
                    "{\"event\":\"session.failed\",\"session_id\":\"s2\",\"reason\":\"nope\",\"exit_code\":2}\n",
                    "this is not json\n",
                ),
            )
            .unwrap();
            let map = terminal_outcomes(&pb).unwrap();
            assert_eq!(map.get("s1"), Some(&Terminal::Done { exit_code: Some(0) }));
            assert_eq!(
                map.get("s2"),
                Some(&Terminal::Failed {
                    reason: "nope".into(),
                    exit_code: Some(2)
                })
            );
            assert!(!map.contains_key("s3"));
        });
    }
}
