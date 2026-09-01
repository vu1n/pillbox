//! Private ACP process/supervisor seam.
//!
//! This module deliberately stops below Huddles orchestration. It owns only
//! bounded ACP framing, wire-request correlation, and the local child/session
//! lifecycle. In particular, `pending` is request/response correlation, not a
//! generic claim or mutex protocol, and no HCP/WorkEvent data is stored here.

use std::{collections::BTreeSet, fmt, time::Duration};

use serde_json::Value;

/// ACP is newline-delimited JSON. Keep one hostile protocol frame bounded.
pub(crate) const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameError {
    Blank,
    Oversized(usize),
    MultipleLines,
    Malformed(String),
    NotObject,
    Serialization(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => write!(f, "ACP frame is blank"),
            Self::Oversized(bytes) => write!(f, "ACP frame is oversized ({bytes} bytes)"),
            Self::MultipleLines => write!(f, "ACP frame contains multiple lines"),
            Self::Malformed(detail) => write!(f, "ACP frame is malformed: {detail}"),
            Self::NotObject => write!(f, "ACP frame must be a JSON object"),
            Self::Serialization(detail) => write!(f, "ACP frame serialization failed: {detail}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Encode exactly one bounded ACP JSON object followed by one newline.
pub(crate) fn encode_frame(value: &Value) -> Result<Vec<u8>, FrameError> {
    if !value.is_object() {
        return Err(FrameError::NotObject);
    }
    let mut frame =
        serde_json::to_vec(value).map_err(|error| FrameError::Serialization(error.to_string()))?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized(frame.len()));
    }
    frame.push(b'\n');
    Ok(frame)
}

/// Decode one line from an ACP NDJSON stream. Blank, malformed, multi-line,
/// non-object, and oversized frames fail closed.
pub(crate) fn decode_frame(line: &[u8]) -> Result<Value, FrameError> {
    let mut payload = line;
    if let Some(without_newline) = payload.strip_suffix(b"\n") {
        payload = without_newline;
        if let Some(without_carriage_return) = payload.strip_suffix(b"\r") {
            payload = without_carriage_return;
        }
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized(payload.len()));
    }
    if payload.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(FrameError::Blank);
    }
    if payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(FrameError::MultipleLines);
    }
    let value = serde_json::from_slice::<Value>(payload)
        .map_err(|error| FrameError::Malformed(error.to_string()))?;
    if !value.is_object() {
        return Err(FrameError::NotObject);
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupervisorState {
    Fresh,
    Spawned,
    Initialized,
    SessionReady,
    TurnActive,
    Cancelling,
    RespawnRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcpError {
    InvalidState {
        expected: &'static str,
        actual: SupervisorState,
    },
    Busy,
    UnknownRequestId(u64),
    RequestIdExhausted,
    CancelTimedOut {
        grace: Duration,
        elapsed: Duration,
    },
}

impl fmt::Display for AcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidState { expected, actual } => {
                write!(f, "ACP supervisor expected {expected}, was {actual:?}")
            }
            Self::Busy => write!(f, "ACP supervisor already has an active turn"),
            Self::UnknownRequestId(id) => write!(f, "ACP response has unknown request id {id}"),
            Self::RequestIdExhausted => write!(f, "ACP request id exhausted"),
            Self::CancelTimedOut { grace, elapsed } => write!(
                f,
                "ACP cancellation cleanup exceeded {:?} (elapsed {:?})",
                grace, elapsed
            ),
        }
    }
}

impl std::error::Error for AcpError {}

/// Correlates ACP responses with requests. This is not scheduling or ownership
/// arbitration; it only prevents an unmatched response from completing a turn.
#[derive(Debug, Default)]
struct RequestTracker {
    next_id: u64,
    pending: BTreeSet<u64>,
}

impl RequestTracker {
    fn issue(&mut self) -> Result<u64, AcpError> {
        let id = self
            .next_id
            .checked_add(1)
            .ok_or(AcpError::RequestIdExhausted)?;
        self.next_id = id;
        self.pending.insert(id);
        Ok(id)
    }

    fn contains(&self, id: u64) -> bool {
        self.pending.contains(&id)
    }

    fn resolve(&mut self, id: u64) -> Result<(), AcpError> {
        if self.pending.remove(&id) {
            Ok(())
        } else {
            Err(AcpError::UnknownRequestId(id))
        }
    }

    fn clear(&mut self) {
        self.pending.clear();
    }
}

/// State machine for one ACP child/session. A crash or cancellation moves to
/// `RespawnRequired`; only the next caller may explicitly spawn a replacement.
#[derive(Debug)]
pub(crate) struct AcpSupervisor {
    state: SupervisorState,
    requests: RequestTracker,
    cancel_grace: Duration,
}

impl AcpSupervisor {
    pub(crate) fn new(cancel_grace: Duration) -> Self {
        Self {
            state: SupervisorState::Fresh,
            requests: RequestTracker::default(),
            cancel_grace,
        }
    }

    pub(crate) fn state(&self) -> SupervisorState {
        self.state
    }

    /// Spawn for the first invocation or for a later invocation after a crash.
    pub(crate) fn spawn_for_invocation(&mut self) -> Result<(), AcpError> {
        match self.state {
            SupervisorState::Fresh | SupervisorState::RespawnRequired => {
                self.state = SupervisorState::Spawned;
                Ok(())
            }
            actual => Err(AcpError::InvalidState {
                expected: "fresh or respawn-required",
                actual,
            }),
        }
    }

    pub(crate) fn mark_initialized(&mut self) -> Result<(), AcpError> {
        self.transition(SupervisorState::Spawned, SupervisorState::Initialized)
    }

    pub(crate) fn open_session(&mut self) -> Result<(), AcpError> {
        self.transition(SupervisorState::Initialized, SupervisorState::SessionReady)
    }

    /// Begin a turn and allocate its ACP request id. A second active turn is
    /// rejected immediately; no prompt is queued.
    pub(crate) fn begin_turn(&mut self) -> Result<u64, AcpError> {
        match self.state {
            SupervisorState::SessionReady => {
                let request_id = self.requests.issue()?;
                self.state = SupervisorState::TurnActive;
                Ok(request_id)
            }
            SupervisorState::TurnActive | SupervisorState::Cancelling => Err(AcpError::Busy),
            actual => Err(AcpError::InvalidState {
                expected: "session-ready",
                actual,
            }),
        }
    }

    pub(crate) fn finish_turn(&mut self, request_id: u64) -> Result<(), AcpError> {
        if self.state != SupervisorState::TurnActive {
            return Err(AcpError::InvalidState {
                expected: "turn-active",
                actual: self.state,
            });
        }
        self.requests.resolve(request_id)?;
        self.state = SupervisorState::SessionReady;
        Ok(())
    }

    /// Mark the child dead. The active request is interrupted and discarded;
    /// it is never replayed by this supervisor.
    pub(crate) fn child_crashed(&mut self) -> bool {
        let interrupted = matches!(
            self.state,
            SupervisorState::TurnActive | SupervisorState::Cancelling
        );
        self.requests.clear();
        self.state = SupervisorState::RespawnRequired;
        interrupted
    }

    pub(crate) fn begin_cancel(&mut self, request_id: u64) -> Result<(), AcpError> {
        if self.state != SupervisorState::TurnActive {
            return Err(AcpError::InvalidState {
                expected: "turn-active",
                actual: self.state,
            });
        }
        if !self.requests.contains(request_id) {
            return Err(AcpError::UnknownRequestId(request_id));
        }
        self.state = SupervisorState::Cancelling;
        Ok(())
    }

    /// Complete the cancel + bounded cleanup/kill sequence. Cleanup always
    /// requires a fresh child, including when its elapsed time exceeds grace.
    pub(crate) fn finish_cancel(
        &mut self,
        request_id: u64,
        elapsed: Duration,
    ) -> Result<(), AcpError> {
        if self.state != SupervisorState::Cancelling {
            return Err(AcpError::InvalidState {
                expected: "cancelling",
                actual: self.state,
            });
        }
        self.requests.resolve(request_id)?;
        self.state = SupervisorState::RespawnRequired;
        if elapsed > self.cancel_grace {
            return Err(AcpError::CancelTimedOut {
                grace: self.cancel_grace,
                elapsed,
            });
        }
        Ok(())
    }

    fn transition(
        &mut self,
        expected: SupervisorState,
        next: SupervisorState,
    ) -> Result<(), AcpError> {
        if self.state != expected {
            return Err(AcpError::InvalidState {
                expected: match expected {
                    SupervisorState::Spawned => "spawned",
                    SupervisorState::Initialized => "initialized",
                    SupervisorState::SessionReady => "session-ready",
                    _ => "expected state",
                },
                actual: self.state,
            });
        }
        self.state = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ndjson_frames_are_bounded_and_single_object_lines() {
        let encoded = encode_frame(&json!({ "id": 1, "method": "session/new" })).unwrap();
        assert_eq!(decode_frame(&encoded).unwrap()["id"], 1);
        assert!(matches!(decode_frame(b"\n"), Err(FrameError::Blank)));
        assert!(matches!(decode_frame(b"[]\n"), Err(FrameError::NotObject)));
        assert!(matches!(
            decode_frame(b"{}\n{}\n"),
            Err(FrameError::MultipleLines)
        ));
        assert!(matches!(decode_frame(b"{"), Err(FrameError::Malformed(_))));
        assert!(matches!(
            decode_frame(&vec![b'x'; MAX_FRAME_BYTES + 1]),
            Err(FrameError::Oversized(_))
        ));
    }

    #[test]
    fn request_ids_correlate_only_to_pending_responses() {
        let mut tracker = RequestTracker::default();
        let first = tracker.issue().unwrap();
        let second = tracker.issue().unwrap();
        assert_eq!((first, second), (1, 2));
        tracker.resolve(first).unwrap();
        assert!(matches!(
            tracker.resolve(first),
            Err(AcpError::UnknownRequestId(1))
        ));
        tracker.resolve(second).unwrap();
    }

    #[test]
    fn lifecycle_rejects_busy_turns_and_finishes_in_session_ready() {
        let mut supervisor = AcpSupervisor::new(Duration::from_secs(1));
        supervisor.spawn_for_invocation().unwrap();
        supervisor.mark_initialized().unwrap();
        supervisor.open_session().unwrap();
        let request_id = supervisor.begin_turn().unwrap();
        assert!(matches!(supervisor.begin_turn(), Err(AcpError::Busy)));
        supervisor.finish_turn(request_id).unwrap();
        assert_eq!(supervisor.state(), SupervisorState::SessionReady);
    }

    #[test]
    fn crash_interrupts_current_turn_and_respawn_is_next_invocation_only() {
        let mut supervisor = AcpSupervisor::new(Duration::from_secs(1));
        supervisor.spawn_for_invocation().unwrap();
        supervisor.mark_initialized().unwrap();
        supervisor.open_session().unwrap();
        let _request_id = supervisor.begin_turn().unwrap();
        assert!(supervisor.child_crashed());
        assert_eq!(supervisor.state(), SupervisorState::RespawnRequired);
        assert!(supervisor.begin_turn().is_err());
        supervisor.spawn_for_invocation().unwrap();
        supervisor.mark_initialized().unwrap();
        supervisor.open_session().unwrap();
        assert!(supervisor.begin_turn().is_ok());
    }

    #[test]
    fn cancellation_requires_bounded_cleanup_and_fresh_child() {
        let mut supervisor = AcpSupervisor::new(Duration::from_millis(50));
        supervisor.spawn_for_invocation().unwrap();
        supervisor.mark_initialized().unwrap();
        supervisor.open_session().unwrap();
        let request_id = supervisor.begin_turn().unwrap();
        supervisor.begin_cancel(request_id).unwrap();
        let error = supervisor
            .finish_cancel(request_id, Duration::from_millis(51))
            .unwrap_err();
        assert!(matches!(error, AcpError::CancelTimedOut { .. }));
        assert_eq!(supervisor.state(), SupervisorState::RespawnRequired);
        supervisor.spawn_for_invocation().unwrap();
    }
}
