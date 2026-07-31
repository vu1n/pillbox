//! Shared one-shot structured-stdout boundary for [`crate::agents::Integration::Structured`]
//! agents (pi, cursor).
//!
//! Each harness owns its JSON wire format; Pillbox maps it immediately into the
//! shared durable contract. Raw harness events are capture input only — never
//! orchestration state and never a PTY transcript.

use std::io::{BufRead as _, Read};

use anyhow::{Context, Result};

use crate::agents::harness::{CursorAdapter, HarnessAdapter, PiAdapter};
use crate::contract::{Actor, Event, Payload, RequestedRunProfile, RunFinished, RunStarted};
use crate::events::log::SessionLog;

pub(crate) struct DrainOutcome {
    pub(crate) events: usize,
    pub(crate) exit_code: i32,
}

/// Build the headless argv for a structured agent given its resolved request.
pub(crate) fn run_argv(
    agent_id: &str,
    requested: Option<RequestedRunProfile>,
    prompt: &str,
) -> Result<Vec<String>> {
    Ok(match agent_id {
        "pi" => {
            let requested = requested.ok_or_else(|| {
                anyhow::anyhow!("pi structured path requires a RequestedRunProfile")
            })?;
            PiAdapter::with_request(requested).run_argv(prompt)
        }
        "cursor" => match requested {
            Some(profile) => CursorAdapter::with_request(profile).run_argv(prompt),
            None => CursorAdapter::default().run_argv(prompt),
        },
        other => anyhow::bail!("structured mode is not wired for agent `{other}`"),
    })
}

/// Persist the canonical start before the guest executes. Later harness
/// "session"/"system init" lines are transport acknowledgement, not a second
/// lifecycle transition.
pub(crate) fn append_started(
    agent_id: &str,
    session_id: &str,
    requested: Option<RequestedRunProfile>,
    log: &mut SessionLog,
) -> Result<()> {
    log.append(&[Event::session(
        session_id,
        Payload::RunStarted(RunStarted {
            agent: agent_id.into(),
            parent_run_id: String::new(),
            base_snapshot: String::new(),
            requested,
        }),
    )
    .with_actor(Actor::agent(agent_id))])?;
    Ok(())
}

/// Normalize one completed structured JSONL capture into the durable session
/// log. The caller has already persisted [`append_started`].
pub(crate) fn drain_jsonl<R: Read>(
    agent_id: &str,
    reader: R,
    session_id: &str,
    requested: Option<RequestedRunProfile>,
    process_exit: i32,
    log: &mut SessionLog,
) -> Result<DrainOutcome> {
    let mut adapter = Adapter::new(agent_id, requested)?;
    let mut total = 0;
    let mut saw_terminal = false;
    let mut terminal_exit = process_exit;

    for (index, line) in std::io::BufReader::new(reader).lines().enumerate() {
        let line = line.with_context(|| format!("read {agent_id} JSONL line {}", index + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str(&line)
            .with_context(|| format!("parse {agent_id} JSONL line {}", index + 1))?;
        let events: Vec<Event> = adapter
            .parse_line(&value)
            .into_iter()
            .filter_map(|payload| match payload {
                Payload::RunStarted(_) => None,
                Payload::RunFinished(mut finished) => {
                    if process_exit != 0 {
                        finished.exit_code = process_exit;
                    }
                    terminal_exit = finished.exit_code;
                    saw_terminal = true;
                    Some(Payload::RunFinished(finished))
                }
                other => Some(other),
            })
            .map(|payload| Event::session(session_id, payload).with_actor(Actor::agent(agent_id)))
            .collect();
        if !events.is_empty() {
            total += events.len();
            log.append(&events)?;
        }
    }

    if !saw_terminal {
        let finished = adapter.terminal_payload(process_exit);
        terminal_exit = finished.exit_code;
        log.append(&[Event::session(session_id, Payload::RunFinished(finished))
            .with_actor(Actor::agent(agent_id))])?;
        total += 1;
    }
    Ok(DrainOutcome {
        events: total,
        exit_code: terminal_exit,
    })
}

/// Close a run that failed before the harness emitted a terminal event or
/// before its capture could be read.
pub(crate) fn append_unavailable_terminal(
    agent_id: &str,
    session_id: &str,
    requested: Option<RequestedRunProfile>,
    exit_code: i32,
    log: &mut SessionLog,
) -> Result<()> {
    let adapter = Adapter::new(agent_id, requested)?;
    log.append(&[Event::session(
        session_id,
        Payload::RunFinished(adapter.terminal_payload(exit_code)),
    )
    .with_actor(Actor::agent(agent_id))])?;
    Ok(())
}

/// Per-agent adapter + terminal payload, shared by drain / unavailable paths.
enum Adapter {
    Pi(PiAdapter),
    Cursor(CursorAdapter),
}

impl Adapter {
    fn new(agent_id: &str, requested: Option<RequestedRunProfile>) -> Result<Self> {
        match agent_id {
            "pi" => {
                let requested = requested.ok_or_else(|| {
                    anyhow::anyhow!("pi structured path requires a RequestedRunProfile")
                })?;
                Ok(Self::Pi(PiAdapter::with_request(requested)))
            }
            "cursor" => Ok(match requested {
                Some(profile) => Self::Cursor(CursorAdapter::with_request(profile)),
                None => Self::Cursor(CursorAdapter::default()),
            }),
            other => anyhow::bail!("structured mode is not wired for agent `{other}`"),
        }
    }

    fn parse_line(&mut self, line: &serde_json::Value) -> Vec<Payload> {
        match self {
            Self::Pi(a) => a.parse_line(line),
            Self::Cursor(a) => a.parse_line(line),
        }
    }

    fn terminal_payload(&self, exit_code: i32) -> RunFinished {
        match self {
            Self::Pi(a) => a.terminal_payload(exit_code),
            Self::Cursor(a) => a.terminal_payload(exit_code),
        }
    }
}
