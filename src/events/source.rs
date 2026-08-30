//! Local read seam for the per-session §0 log.
//!
//! Managed execution evidence is copied into the ordinary local log. Reads do
//! not open a remote replay stream or depend on Durable Object storage.

use std::sync::atomic::AtomicBool;

use anyhow::Result;

use super::log::SessionLog;
use crate::contract::Event;
use crate::pillbox::Pillbox;

pub(crate) trait EventSource {
    fn subscribe(
        &self,
        from: u64,
        stop: &AtomicBool,
        sink: &mut dyn FnMut(&Event) -> bool,
    ) -> Result<()>;
}

impl EventSource for SessionLog {
    fn subscribe(
        &self,
        from: u64,
        stop: &AtomicBool,
        sink: &mut dyn FnMut(&Event) -> bool,
    ) -> Result<()> {
        SessionLog::subscribe(self, from, stop, sink)
    }
}

pub(crate) fn open_event_source(
    pb: &Pillbox,
    session_id: &str,
) -> Result<Box<dyn EventSource + Send>> {
    Ok(Box::new(SessionLog::open(pb, session_id)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Payload, ToolCall, ToolStatus};
    use crate::test_util::with_isolated_home;

    #[test]
    fn open_event_source_is_always_local() {
        with_isolated_home("source-local-only", || {
            std::env::set_var("PILLBOX_MANAGED_DO_URL", "https://retired.invalid");
            let pb = crate::pillbox::global();
            SessionLog::open(&pb, "sess-local")
                .unwrap()
                .append(&[Event::session(
                    "sess-local",
                    Payload::ToolCall(ToolCall {
                        tool_call_id: "tc-a".into(),
                        name: "a".into(),
                        status: ToolStatus::Running,
                        input: None,
                        output: String::new(),
                        title: String::new(),
                    }),
                )])
                .unwrap();
            let source = open_event_source(&pb, "sess-local").unwrap();
            let stop = AtomicBool::new(false);
            let mut seen = Vec::new();
            source
                .subscribe(1, &stop, &mut |event| {
                    seen.push(event.seq);
                    false
                })
                .unwrap();
            assert_eq!(seen, vec![1]);
            std::env::remove_var("PILLBOX_MANAGED_DO_URL");
        });
    }
}
