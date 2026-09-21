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
use serde::{Deserialize, Serialize};

use crate::contract::{
    AttentionReason, EffectiveRuntimeLimitsEvidence, Payload, Role, ServedRunProfile,
    ServedRunProfileEvidence, ToolStatus,
};
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
    /// `at` of the log event that last flipped the agent between producing
    /// and awaiting input — the transition time behind the non-terminal
    /// [`Condition`]s. Empty when the log carried no such event.
    pub(crate) status_at: String,
    pub(crate) log_seq: u64,
    /// Latest model the runtime reported on a completed assistant message.
    /// This is response evidence, distinct from the model requested at launch.
    pub(crate) served_model: Option<SourcedEvidence<ServedRunProfileEvidence>>,
    /// Latest effective-limit evidence reported by the runtime. Advertised
    /// capability and the requested profile never populate this field.
    pub(crate) effective_limits: Option<SourcedEvidence<EffectiveRuntimeLimitsEvidence>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourcedEvidence<T> {
    pub(crate) evidence: T,
    pub(crate) seq: u64,
}

fn served_profile_from_message(model: String) -> ServedRunProfileEvidence {
    let (provider, model) = match model.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider.to_string()), model.to_string())
        }
        _ => (None, model),
    };
    ServedRunProfileEvidence::Reported {
        profile: ServedRunProfile {
            provider,
            model,
            profile: None,
            reasoning_profile: None,
        },
    }
}

/// A host-visible terminal outcome for a session, with the detail `diagnose`
/// surfaces (the same single parse serves both `list` status and the readout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Terminal {
    Done {
        exit_code: Option<i64>,
        /// The lifecycle line's `ended_at`, when it carried one.
        at: Option<String>,
    },
    Failed {
        reason: String,
        exit_code: Option<i64>,
        at: Option<String>,
    },
}

impl Terminal {
    fn at(&self) -> Option<&str> {
        match self {
            Terminal::Done { at, .. } | Terminal::Failed { at, .. } => at.as_deref(),
        }
    }
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
    #[serde(default)]
    ended_at: Option<String>,
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
                at: parsed.ended_at,
            },
            "session.failed" => Terminal::Failed {
                reason: parsed.reason.unwrap_or_default(),
                exit_code: parsed.exit_code,
                at: parsed.ended_at,
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
    let mut status_at = String::new();
    let mut assistant_turns = 0;
    let mut tool_calls = 0;
    let mut last_at = String::new();
    let mut log_seq = 0;
    let mut served_model = None;
    let mut effective_limits = None;
    for ev in log::read_log(pb, &session.id)? {
        log_seq = ev.seq;
        if !ev.at.is_empty() {
            last_at = ev.at;
        }
        // Record the transition time only when the resting state actually
        // flips, so a burst of deltas doesn't keep bumping "since when".
        let mut set_pending = |next: bool, status_at: &mut String| {
            if pending_input != next {
                pending_input = next;
                *status_at = last_at.clone();
            }
        };
        match ev.payload {
            Payload::AttentionRequired(a) if a.reason == AttentionReason::NeedsInput => {
                set_pending(true, &mut status_at);
            }
            Payload::MessageStart(m) => {
                set_pending(false, &mut status_at);
                if m.role == Role::Assistant {
                    assistant_turns += 1;
                }
            }
            // A tool call lands twice (Running, then its correlated result);
            // count the Running side so the number is invocations, not events.
            Payload::ToolCall(t) if t.status == ToolStatus::Running => {
                set_pending(false, &mut status_at);
                tool_calls += 1;
            }
            Payload::Thinking(_) => set_pending(false, &mut status_at),
            Payload::MessageEnd(m) if !m.model.is_empty() => {
                served_model = Some(SourcedEvidence {
                    evidence: served_profile_from_message(m.model),
                    seq: ev.seq,
                });
            }
            Payload::RunFinished(run) => {
                if let Some(evidence) = run.served_model {
                    served_model = Some(SourcedEvidence {
                        evidence,
                        seq: ev.seq,
                    });
                }
                if let Some(evidence) = run.effective_limits {
                    effective_limits = Some(SourcedEvidence {
                        evidence,
                        seq: ev.seq,
                    });
                }
            }
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
        status_at,
        log_seq,
        served_model,
        effective_limits,
    })
}

/// One named, typed fact about a session, in the Kubernetes condition shape
/// (`type` / `status` / `reason` / `message` / `lastTransitionTime`): the one
/// stable field an orchestrator branches on instead of parsing the status label
/// or the human readout. Emitted on `session info|diagnose|list --json` as
/// `conditions[]`.
///
/// The set is small on purpose and each answers one question:
///
/// | type | True when | the one to wait on for… |
/// |---|---|---|
/// | `Ready` | the session is live and driveable (not terminal) | — |
/// | `AwaitingInput` | the agent ended its turn and is waiting to be driven | a `send` → `wait-idle` loop |
/// | `Finished` | a host-visible terminal outcome exists | a one-shot run |
/// | `ResultAvailable` | a result snapshot is recorded on the session | `session pull` |
///
/// `status` is `"True"` / `"False"` (never `"Unknown"`: the deriver's honesty
/// rule is that a host-invisible fact reads as its resting value, see the
/// module doc). `last_transition_time` is the `at` of the event that produced
/// the current value when the log carries one, else absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Condition {
    pub(crate) r#type: &'static str,
    pub(crate) status: &'static str,
    pub(crate) reason: &'static str,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_transition_time: Option<String>,
}

impl Condition {
    fn new(
        r#type: &'static str,
        truth: bool,
        reason: &'static str,
        message: impl Into<String>,
        at: Option<&str>,
    ) -> Self {
        Condition {
            r#type,
            status: if truth { "True" } else { "False" },
            reason,
            message: message.into(),
            last_transition_time: at.filter(|a| !a.is_empty()).map(str::to_string),
        }
    }
}

/// Derive the [`Condition`] set from a [`summarize`] fold — the same fold `list`,
/// `info` and `diagnose` already run, so the conditions can never disagree with
/// the status label.
pub(crate) fn conditions(
    session: &Session,
    d: &Diagnosis,
    terminal: Option<&Terminal>,
) -> Vec<Condition> {
    let terminal_at = terminal.and_then(Terminal::at);
    // Non-terminal transitions come from the per-session log; a session with
    // no log yet has only its launch time.
    let live_at = if d.status_at.is_empty() {
        session.started_at.as_str()
    } else {
        d.status_at.as_str()
    };
    let exit_suffix = |code: Option<i64>| code.map(|c| format!(" (exit {c})")).unwrap_or_default();
    let drive_hint = format!(
        "the agent ended its turn; drive it with `pillbox session send {} …`",
        session.id
    );

    let (ready, awaiting, finished) = match d.status {
        SessionStatus::Running => (
            Condition::new(
                "Ready",
                true,
                "Running",
                "the agent is producing output or running tools",
                Some(live_at),
            ),
            Condition::new(
                "AwaitingInput",
                false,
                "Busy",
                "the agent is mid-turn",
                Some(live_at),
            ),
            Condition::new(
                "Finished",
                false,
                "InProgress",
                "no host-visible terminal outcome yet",
                None,
            ),
        ),
        SessionStatus::NeedsInput => (
            Condition::new(
                "Ready",
                true,
                "AwaitingInput",
                drive_hint.clone(),
                Some(live_at),
            ),
            Condition::new(
                "AwaitingInput",
                true,
                "TurnEnded",
                drive_hint.clone(),
                Some(live_at),
            ),
            Condition::new(
                "Finished",
                false,
                "InProgress",
                "no host-visible terminal outcome yet",
                None,
            ),
        ),
        SessionStatus::Done => {
            let (message, at) = match terminal {
                Some(Terminal::Done { exit_code, at }) => (
                    format!("completed{}", exit_suffix(*exit_code)),
                    at.as_deref(),
                ),
                // `Done` without a lifecycle line: the result snapshot alone
                // proved completion (see `summarize`'s precedence).
                _ => ("completed (result snapshot recorded)".to_string(), None),
            };
            (
                Condition::new("Ready", false, "Completed", message.clone(), at),
                Condition::new("AwaitingInput", false, "Finished", message.clone(), at),
                Condition::new("Finished", true, "Completed", message, at),
            )
        }
        SessionStatus::Failed => {
            let (message, at) = match terminal {
                Some(Terminal::Failed {
                    reason,
                    exit_code,
                    at,
                }) => (
                    format!(
                        "{}{}",
                        if reason.is_empty() {
                            "failed"
                        } else {
                            reason.as_str()
                        },
                        exit_suffix(*exit_code)
                    ),
                    at.as_deref(),
                ),
                _ => ("failed".to_string(), None),
            };
            (
                Condition::new("Ready", false, "Failed", message.clone(), at),
                Condition::new("AwaitingInput", false, "Finished", message.clone(), at),
                Condition::new("Finished", true, "Failed", message, at),
            )
        }
    };
    let result = match &session.result_snapshot {
        Some(handle) => Condition::new(
            "ResultAvailable",
            true,
            "SnapshotPushed",
            format!(
                "result snapshot {handle}; rehydrate with `pillbox session pull {}`",
                session.id
            ),
            terminal_at,
        ),
        None => Condition::new(
            "ResultAvailable",
            false,
            "NotPushed",
            "no result snapshot on the record yet",
            None,
        ),
    };
    vec![ready, awaiting, finished, result]
}

/// Status-only view for `list`/`info` — [`summarize`] then the status.
pub(crate) fn derive(
    pb: &Pillbox,
    session: &Session,
    terminal: Option<&Terminal>,
) -> Result<SessionStatus> {
    Ok(summarize(pb, session, terminal)?.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{AttentionRequired, Event, MessageEnd, MessageStart, Role};
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
                at: None,
            };
            let done = Terminal::Done {
                exit_code: Some(0),
                at: None,
            };
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
    fn conditions_follow_the_status_fold() {
        with_isolated_home("status-conditions", || {
            let pb = crate::pillbox::global();
            let mut s = sess("ffff11112222");
            s.started_at = "2026-01-01T00:00:00Z".into();
            let by_type =
                |c: &[Condition], t: &str| c.iter().find(|c| c.r#type == t).cloned().expect(t);

            // Fresh record, no log: live, not awaiting, not finished, no result —
            // and the only transition time the host knows is the launch.
            let d = summarize(&pb, &s, None).unwrap();
            let c = conditions(&s, &d, None);
            assert_eq!(c.len(), 4);
            let ready = by_type(&c, "Ready");
            assert_eq!((ready.status, ready.reason), ("True", "Running"));
            assert_eq!(
                ready.last_transition_time.as_deref(),
                Some("2026-01-01T00:00:00Z")
            );
            assert_eq!(by_type(&c, "AwaitingInput").status, "False");
            assert_eq!(by_type(&c, "Finished").status, "False");
            assert_eq!(by_type(&c, "ResultAvailable").reason, "NotPushed");

            // A turn ends awaiting input: AwaitingInput flips True, stamped with
            // the attention event's `at`, and the message carries the drive verb.
            let mut log = SessionLog::open(&pb, &s.id).unwrap();
            log.append(&[
                Event::session(&s.id, msg(Role::Assistant)),
                Event::session(&s.id, needs_input()),
            ])
            .unwrap();
            let d = summarize(&pb, &s, None).unwrap();
            assert!(!d.status_at.is_empty(), "the flip records its event time");
            let c = conditions(&s, &d, None);
            let awaiting = by_type(&c, "AwaitingInput");
            assert_eq!((awaiting.status, awaiting.reason), ("True", "TurnEnded"));
            assert_eq!(
                awaiting.last_transition_time.as_deref(),
                Some(d.status_at.as_str())
            );
            assert!(awaiting
                .message
                .contains("pillbox session send ffff11112222"));
            assert_eq!(by_type(&c, "Ready").reason, "AwaitingInput");

            // A host-visible failure: Ready False, Finished True/Failed with the
            // reason + exit code, stamped with the lifecycle line's ended_at.
            let failed = Terminal::Failed {
                reason: "boom".into(),
                exit_code: Some(3),
                at: Some("2026-01-01T00:01:00Z".into()),
            };
            let d = summarize(&pb, &s, Some(&failed)).unwrap();
            let c = conditions(&s, &d, Some(&failed));
            let finished = by_type(&c, "Finished");
            assert_eq!((finished.status, finished.reason), ("True", "Failed"));
            assert_eq!(finished.message, "boom (exit 3)");
            assert_eq!(
                finished.last_transition_time.as_deref(),
                Some("2026-01-01T00:01:00Z")
            );
            assert_eq!(by_type(&c, "Ready").status, "False");
            assert_eq!(by_type(&c, "AwaitingInput").status, "False");

            // A pushed result: ResultAvailable True with the handle and pull verb.
            s.result_snapshot = Some("abc123".into());
            let d = summarize(&pb, &s, None).unwrap();
            let c = conditions(&s, &d, None);
            let result = by_type(&c, "ResultAvailable");
            assert_eq!((result.status, result.reason), ("True", "SnapshotPushed"));
            assert!(result.message.contains("abc123") && result.message.contains("session pull"));
            // …and the snapshot alone proves completion (no lifecycle line).
            assert_eq!(
                by_type(&c, "Finished").message,
                "completed (result snapshot recorded)"
            );

            // The JSON shape is the Kubernetes one; an absent time is omitted.
            let v = serde_json::to_value(by_type(&c, "Finished")).unwrap();
            assert_eq!(v["type"], "Finished");
            assert_eq!(v["status"], "True");
            assert!(v.get("last_transition_time").is_none());
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
    fn summarize_retains_latest_reported_served_model_and_event_seq() {
        with_isolated_home("status-served-model", || {
            let pb = crate::pillbox::global();
            let s = sess("ffff11112222");
            let mut log = SessionLog::open(&pb, &s.id).unwrap();
            let mut first = MessageEnd::new("m1");
            first.model = "openai/gpt-5.6-luna".into();
            let unavailable = MessageEnd::new("m2");
            let mut latest = MessageEnd::new("m3");
            latest.model = "openai/gpt-5.6-terra".into();
            log.append(&[
                Event::session(&s.id, Payload::MessageEnd(first)),
                Event::session(&s.id, Payload::MessageEnd(unavailable)),
                Event::session(&s.id, Payload::MessageEnd(latest)),
            ])
            .unwrap();

            let d = summarize(&pb, &s, None).unwrap();
            assert_eq!(
                d.served_model,
                Some(SourcedEvidence {
                    evidence: ServedRunProfileEvidence::Reported {
                        profile: ServedRunProfile {
                            provider: Some("openai".into()),
                            model: "gpt-5.6-terra".into(),
                            profile: None,
                            reasoning_profile: None,
                        },
                    },
                    seq: 3,
                })
            );
        });
    }

    #[test]
    fn model_profile_contract_runtime_evidence_is_sourced_and_session_isolated() {
        with_isolated_home("status-runtime-profile", || {
            let pb = crate::pillbox::global();
            let session_a = sess("aaaa11112222");
            let session_b = sess("bbbb11112222");
            let finished = |model: &str, window| {
                Payload::RunFinished(crate::contract::RunFinished {
                    result_snapshot: String::new(),
                    exit_code: 0,
                    served_model: Some(ServedRunProfileEvidence::Reported {
                        profile: ServedRunProfile {
                            provider: Some("openai".into()),
                            model: model.into(),
                            profile: None,
                            reasoning_profile: Some("high".into()),
                        },
                    }),
                    effective_limits: Some(EffectiveRuntimeLimitsEvidence::Reported {
                        limits: crate::contract::EffectiveRuntimeLimits {
                            context_window_tokens: Some(window),
                            max_output_tokens: None,
                            supported_reasoning_profiles: vec!["high".into()],
                        },
                    }),
                })
            };
            SessionLog::open(&pb, &session_a.id)
                .unwrap()
                .append(&[Event::session(
                    &session_a.id,
                    finished("gpt-5.6-sol", 200_000),
                )])
                .unwrap();
            SessionLog::open(&pb, &session_b.id)
                .unwrap()
                .append(&[Event::session(
                    &session_b.id,
                    finished("gpt-5.6-terra", 128_000),
                )])
                .unwrap();

            let a = summarize(&pb, &session_a, None).unwrap();
            let b = summarize(&pb, &session_b, None).unwrap();
            assert_eq!(a.served_model.as_ref().unwrap().seq, 1);
            assert_eq!(b.served_model.as_ref().unwrap().seq, 1);
            assert_ne!(a.served_model, b.served_model);
            assert_ne!(a.effective_limits, b.effective_limits);
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
                    "{\"event\":\"session.completed\",\"session_id\":\"s1\",\"exit_code\":0,\"ended_at\":\"2026-01-01T00:00:09Z\"}\n",
                    "{\"event\":\"session.failed\",\"session_id\":\"s2\",\"reason\":\"nope\",\"exit_code\":2}\n",
                    "this is not json\n",
                ),
            )
            .unwrap();
            let map = terminal_outcomes(&pb).unwrap();
            assert_eq!(
                map.get("s1"),
                Some(&Terminal::Done {
                    exit_code: Some(0),
                    at: Some("2026-01-01T00:00:09Z".into()),
                })
            );
            assert_eq!(
                map.get("s2"),
                Some(&Terminal::Failed {
                    reason: "nope".into(),
                    exit_code: Some(2),
                    at: None,
                })
            );
            assert!(!map.contains_key("s3"));
        });
    }
}
