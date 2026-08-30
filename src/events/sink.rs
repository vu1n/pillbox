//! Local write seam for the per-session §0 log.
//!
//! Managed execution returns bounded evidence to the caller, which appends it
//! here. Environment variables cannot redirect the session log to a remote
//! sequencer; multiplayer ordering belongs to an external orchestrator.

use anyhow::Result;

use super::log::SessionLog;
use crate::contract::Event;
use crate::pillbox::Pillbox;

pub(crate) trait EventLog {
    fn append(&mut self, events: &[Event]) -> Result<u64>;
}

impl EventLog for SessionLog {
    fn append(&mut self, events: &[Event]) -> Result<u64> {
        SessionLog::append(self, events)
    }
}

pub(crate) fn open_event_log(pb: &Pillbox, session_id: &str) -> Result<Box<dyn EventLog + Send>> {
    Ok(Box::new(SessionLog::open(pb, session_id)?))
}

pub(crate) fn open_or_warn(pb: &Pillbox, session_id: &str) -> Option<Box<dyn EventLog + Send>> {
    match open_event_log(pb, session_id) {
        Ok(log) => Some(log),
        Err(error) => {
            eprintln!("pillbox: warning: couldn't open session log: {error:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Payload, ToolCall, ToolStatus};
    use crate::test_util::with_isolated_home;

    #[test]
    fn open_event_log_is_always_local() {
        with_isolated_home("sink-local-only", || {
            std::env::set_var("PILLBOX_MANAGED_DO_URL", "https://retired.invalid");
            let pb = crate::pillbox::global();
            let mut sink = open_event_log(&pb, "sess-local").unwrap();
            let payload = Payload::ToolCall(ToolCall {
                tool_call_id: "tc-a".into(),
                name: "a".into(),
                status: ToolStatus::Running,
                input: None,
                output: String::new(),
                title: String::new(),
            });
            assert_eq!(
                sink.append(&[Event::session("sess-local", payload)])
                    .unwrap(),
                1
            );
            assert_eq!(SessionLog::open(&pb, "sess-local").unwrap().last_seq(), 1);
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
        });
    }
}
