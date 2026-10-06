//! Shared one-shot structured-stdout boundary for [`crate::agents::Integration::Structured`]
//! agents (pi, cursor, claude-stream).
//!
//! Each harness owns its JSON wire format; Pillbox maps it immediately into the
//! shared durable contract. Raw harness events are capture input only — never
//! orchestration state and never a PTY transcript.

use std::io::{BufRead as _, Read};

use anyhow::{Context, Result};

use crate::agents::harness::{ClaudeAdapter, CursorAdapter, HarnessAdapter, PiAdapter};
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
        // The libkrun guest runs as root: never the docker adapter's
        // `--dangerously-skip-permissions` argv (claude refuses it as root).
        "claude-stream" => ClaudeAdapter::with_request(requested)
            .guest_root_argv(crate::agents::CLAUDE_STREAM.sandbox_args, prompt),
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
    Claude(ClaudeAdapter),
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
            "claude-stream" => Ok(Self::Claude(ClaudeAdapter::with_request(requested))),
            other => anyhow::bail!("structured mode is not wired for agent `{other}`"),
        }
    }

    fn parse_line(&mut self, line: &serde_json::Value) -> Vec<Payload> {
        match self {
            Self::Pi(a) => a.parse_line(line),
            Self::Cursor(a) => a.parse_line(line),
            Self::Claude(a) => a.parse_line(line),
        }
    }

    fn terminal_payload(&self, exit_code: i32) -> RunFinished {
        match self {
            Self::Pi(a) => a.terminal_payload(exit_code),
            Self::Cursor(a) => a.terminal_payload(exit_code),
            Self::Claude(a) => a.terminal_payload(exit_code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `claude -p --output-format stream-json --verbose`
    /// capture of Claude Code 2.1.289 (the runner pin) against a loopback
    /// Anthropic-API mock.
    const CLAUDE_2_1_289_CAPTURE: &str = r#"{"type":"system","subtype":"init","cwd":"/workspace/app","session_id":"a813","model":"claude-mock-1","permissionMode":"auto","apiKeySource":"ANTHROPIC_API_KEY","claude_code_version":"2.1.289"}
{"type":"assistant","message":{"id":"msg_mock_1","type":"message","role":"assistant","model":"claude-mock-1","content":[{"type":"text","text":"Running it."}]},"parent_tool_use_id":null,"session_id":"a813"}
{"type":"assistant","message":{"id":"msg_mock_1","type":"message","role":"assistant","model":"claude-mock-1","content":[{"type":"tool_use","id":"toolu_mock_1","name":"Bash","input":{"command":"echo HELLO","description":"say hello"}}]},"parent_tool_use_id":null,"session_id":"a813"}
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_mock_1","type":"tool_result","content":"HELLO","is_error":false}]},"parent_tool_use_id":null,"session_id":"a813","tool_use_result":{"stdout":"HELLO","stderr":"","interrupted":false}}
{"type":"assistant","message":{"id":"msg_mock_2","type":"message","role":"assistant","model":"claude-mock-1","content":[{"type":"text","text":"done"}]},"parent_tool_use_id":null,"session_id":"a813"}
{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"done","total_cost_usd":0.00056,"permission_denials":[],"terminal_reason":"completed","session_id":"a813"}
"#;

    fn drain(capture: &str, process_exit: i32) -> (DrainOutcome, Vec<Event>) {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::open_at(dir.path().to_path_buf()).unwrap();
        append_started("claude-stream", "s1", None, &mut log).unwrap();
        let outcome = drain_jsonl(
            "claude-stream",
            capture.as_bytes(),
            "s1",
            None,
            process_exit,
            &mut log,
        )
        .unwrap();
        let events = log.read_from(0).unwrap();
        (outcome, events)
    }

    #[test]
    fn claude_stream_argv_is_root_safe() {
        let argv = run_argv("claude-stream", None, "-x fix it").unwrap();
        assert!(!argv.iter().any(|a| a == "--dangerously-skip-permissions"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--permission-mode" && w[1] == "auto"));
        assert_eq!(&argv[argv.len() - 2..], ["--", "-x fix it"]);
    }

    #[test]
    fn claude_stream_capture_drains_into_the_session_log() {
        let (outcome, events) = drain(CLAUDE_2_1_289_CAPTURE, 0);
        assert_eq!(outcome.exit_code, 0);
        // The canonical start is the only RunStarted; the harness init is not
        // a second lifecycle transition.
        let starts = events
            .iter()
            .filter(|e| matches!(e.payload, Payload::RunStarted(_)))
            .count();
        assert_eq!(starts, 1);
        let tools = events
            .iter()
            .filter(|e| matches!(e.payload, Payload::ToolCall(_)))
            .count();
        assert_eq!(tools, 2);
        assert!(events
            .iter()
            .any(|e| matches!(&e.payload, Payload::RunFinished(r) if r.exit_code == 0)));
        assert!(matches!(
            events.last().map(|e| &e.payload),
            Some(Payload::Custom(c)) if c.name == "usage"
        ));
        assert_eq!(outcome.events + 1, events.len());
    }

    #[test]
    fn claude_stream_nonzero_exit_fails_and_missing_result_is_closed() {
        let (outcome, _) = drain(CLAUDE_2_1_289_CAPTURE, 137);
        assert_eq!(outcome.exit_code, 137);
        // Killed before the `result` line: the drain still closes the run.
        let truncated = CLAUDE_2_1_289_CAPTURE
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n");
        let (outcome, events) = drain(&truncated, 1);
        assert_eq!(outcome.exit_code, 1);
        assert!(matches!(
            events.last().map(|e| &e.payload),
            Some(Payload::RunFinished(r)) if r.exit_code == 1
        ));
    }
}
