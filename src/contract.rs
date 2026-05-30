//! The agent I/O contract — Rust side.
//!
//! `proto/pillbox/v1/agent.proto` is the canonical cross-language spec
//! (consumers codegen from it). pillbox itself is sync + serde, and v1
//! transports are JSON (stdio / webhook / in-proc callback), so we hand-write
//! the contract as serde types here rather than dragging prost/protoc into
//! the core. The wire shape is ergonomic JSON — camelCase fields, a `type`
//! discriminator on the payload, snake_case enums — a faithful (not
//! byte-identical) encoding of the proto. The strict protobuf-JSON mapping is
//! the gRPC `serve` path's job (later, feature-gated), not this one.
//!
//! Producers build [`Event`]s and push them to an [`EventSink`]. The
//! sandbox/exec runtime (next slice) is the first producer.

// Contract surface lands ahead of its first producer (contract-first).
#![allow(dead_code)]

use std::io::Write;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One event on a sandbox's stream. Envelope + a typed payload.
///
/// `seq` is monotonic per *emitter* (the per-run/exec `EventEmitter` counter,
/// not pillbox-wide) and assigned to DURABLE events only; ephemeral telemetry
/// carries `seq == 0` and is excluded from replay. (vNext moves this to a
/// per-session, gateway-assigned seq — see docs/session-event-log.md.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Event {
    pub(crate) seq: u64,
    pub(crate) sandbox_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) exec_id: String,
    /// RFC3339 timestamp.
    pub(crate) at: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) ephemeral: bool,
    pub(crate) payload: Payload,
}

impl Event {
    /// Durable event (`seq` assigned by the caller's counter).
    pub(crate) fn durable(seq: u64, sandbox_id: impl Into<String>, payload: Payload) -> Self {
        Self {
            seq,
            sandbox_id: sandbox_id.into(),
            run_id: String::new(),
            exec_id: String::new(),
            at: crate::session::now_rfc3339(),
            ephemeral: false,
            payload,
        }
    }

    pub(crate) fn with_run(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = run_id.into();
        self
    }

    pub(crate) fn with_exec(mut self, exec_id: impl Into<String>) -> Self {
        self.exec_id = exec_id.into();
        self
    }
}

/// Typed payload. Internally tagged on `type` (snake_case), so an
/// `exec_output` serializes as `{"type":"exec_output","stream":"stdout",...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Payload {
    // sandbox lifecycle
    SandboxProvisioned(SandboxProvisioned),
    SandboxReady,
    SandboxDestroyed(SandboxDestroyed),
    // agent run lifecycle
    RunStarted(RunStarted),
    RunFinished(RunFinished),
    RunFailed(RunFailed),
    // agent output (normalized)
    MessageStart(MessageStart),
    MessageDelta(MessageDelta),
    MessageEnd(MessageEnd),
    ToolCall(ToolCall),
    // ephemeral card telemetry
    PhaseChanged(PhaseChanged),
    TodosUpdated(TodosUpdated),
    // human-in-the-loop
    PermissionRequested(PermissionRequested),
    PermissionResolved(PermissionResolved),
    AttentionRequired(AttentionRequired),
    // workspace
    Checkpoint(Checkpoint),
    ResultReady(ResultReady),
    // exec channel
    ExecStarted(ExecStarted),
    ExecOutput(ExecOutput),
    ExecExit(ExecExit),
    // extension valve
    Custom(Custom),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxProvisioned {
    pub(crate) remote: String,
    pub(crate) image: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxDestroyed {
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunStarted {
    pub(crate) agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) parent_run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) base_snapshot: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunFinished {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) result_snapshot: String,
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunFailed {
    pub(crate) reason: String,
    pub(crate) exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageStart {
    pub(crate) message_id: String,
    pub(crate) role: Role,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageDelta {
    pub(crate) message_id: String,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageEnd {
    pub(crate) message_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolCall {
    pub(crate) tool_call_id: String,
    pub(crate) name: String,
    pub(crate) status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) output: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhaseChanged {
    pub(crate) phase: AgentPhase,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Todo {
    pub(crate) text: String,
    pub(crate) status: TodoStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TodosUpdated {
    pub(crate) todos: Vec<Todo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionRequested {
    pub(crate) permission_id: String,
    pub(crate) tool: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionResolved {
    pub(crate) permission_id: String,
    pub(crate) decision: PermissionDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttentionRequired {
    pub(crate) reason: AttentionReason,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Checkpoint {
    pub(crate) snapshot_handle: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResultReady {
    pub(crate) snapshot_handle: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecStarted {
    pub(crate) argv: Vec<String>,
}

/// `data` is base64-encoded bytes (matches the proto `bytes` field under
/// protobuf-JSON) so binary stdout survives the wire intact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecOutput {
    pub(crate) stream: StdStream,
    pub(crate) data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecExit {
    pub(crate) code: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Custom {
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<Value>,
}

// ── Enums (snake_case on the wire; `Unspecified` is the deserialize fallback) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Unspecified,
    Assistant,
    User,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolStatus {
    Unspecified,
    Running,
    Completed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentPhase {
    Unspecified,
    Queued,
    Thinking,
    Editing,
    RunningTool,
    WaitingInput,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TodoStatus {
    Unspecified,
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StdStream {
    Unspecified,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttentionReason {
    Unspecified,
    NeedsInput,
    ErrorStalled,
    Permission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PermissionDecision {
    Unspecified,
    Allow,
    AllowAlways,
    Deny,
}

// ── Sinks ──────────────────────────────────────────────────────────────────

/// Where a producer ships events. The first transport is JSONL (stdio); the
/// existing webhook sink in `events::webhook` is the second producer target.
pub(crate) trait EventSink {
    fn emit(&mut self, event: &Event) -> Result<()>;
}

/// One JSON object per line. Drives the stdio/subprocess transport — a
/// consumer reads lines and parses each into an [`Event`].
pub(crate) struct JsonlSink<W: Write> {
    writer: W,
}

impl<W: Write> JsonlSink<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write> EventSink for JsonlSink<W> {
    fn emit(&mut self, event: &Event) -> Result<()> {
        let line = serde_json::to_string(event).context("serialize event")?;
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.write_all(b"\n"))
            .and_then(|()| self.writer.flush())
            .context("write event line")
    }
}

/// What a run's events correlate to — an agent `run` or a one-off `exec`.
/// Selects which id the emitter stamps on each [`Event`].
pub(crate) enum Correlation {
    Run(String),
    Exec(String),
}

/// Per-run event emission: assigns the monotonic durable `seq`, stamps the
/// sandbox + correlation id, and pushes to a sink. The single place that
/// logic lives — shared by the exec path and both agent drivers.
pub(crate) struct EventEmitter {
    sink: Box<dyn EventSink>,
    sandbox_id: String,
    correlation: Correlation,
    seq: u64,
}

impl EventEmitter {
    pub(crate) fn new(
        sink: Box<dyn EventSink>,
        sandbox_id: String,
        correlation: Correlation,
    ) -> Self {
        Self {
            sink,
            sandbox_id,
            correlation,
            seq: 0,
        }
    }

    pub(crate) fn emit(&mut self, payload: Payload) -> Result<()> {
        self.seq += 1;
        let event = Event::durable(self.seq, &self.sandbox_id, payload);
        let event = match &self.correlation {
            Correlation::Run(id) => event.with_run(id),
            Correlation::Exec(id) => event.with_exec(id),
        };
        self.sink.emit(&event)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reparse(event: &Event) -> Event {
        let s = serde_json::to_string(event).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn exec_output_round_trips_and_uses_type_tag() {
        let ev = Event::durable(
            7,
            "sb-1",
            Payload::ExecOutput(ExecOutput {
                stream: StdStream::Stdout,
                data: "aGVsbG8=".into(),
            }),
        )
        .with_exec("ex-1");
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"exec_output""#), "{s}");
        assert!(s.contains(r#""stream":"stdout""#), "{s}");
        assert!(s.contains(r#""execId":"ex-1""#), "{s}");
        assert_eq!(reparse(&ev), ev);
    }

    #[test]
    fn unit_variant_serializes_as_bare_type() {
        let ev = Event::durable(1, "sb-1", Payload::SandboxReady);
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""payload":{"type":"sandbox_ready"}"#), "{s}");
        assert_eq!(reparse(&ev), ev);
    }

    #[test]
    fn tool_call_carries_arbitrary_json_input() {
        let ev = Event::durable(
            3,
            "sb-1",
            Payload::ToolCall(ToolCall {
                tool_call_id: "tc-1".into(),
                name: "Bash".into(),
                status: ToolStatus::Running,
                input: Some(json!({"command": "ls -la"})),
                output: String::new(),
                title: "running ls".into(),
            }),
        )
        .with_run("run-1");
        let back = reparse(&ev);
        assert_eq!(back, ev);
        let Payload::ToolCall(tc) = back.payload else {
            panic!("wrong variant")
        };
        assert_eq!(tc.input.unwrap()["command"], "ls -la");
    }

    #[test]
    fn ephemeral_false_and_empty_ids_are_omitted() {
        let ev = Event::durable(2, "sb-1", Payload::SandboxReady);
        let s = serde_json::to_string(&ev).unwrap();
        assert!(!s.contains("ephemeral"), "{s}");
        assert!(!s.contains("runId"), "{s}");
        assert!(!s.contains("execId"), "{s}");
    }

    #[test]
    fn jsonl_sink_writes_one_parseable_line_per_event() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut sink = JsonlSink::new(&mut buf);
            sink.emit(&Event::durable(1, "sb", Payload::SandboxReady))
                .unwrap();
            sink.emit(&Event::durable(
                2,
                "sb",
                Payload::ExecExit(ExecExit { code: 0 }),
            ))
            .unwrap();
        }
        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_str::<Event>(line).unwrap();
        }
    }

    #[test]
    fn enums_render_snake_case() {
        assert_eq!(
            serde_json::to_string(&StdStream::Stderr).unwrap(),
            r#""stderr""#
        );
        assert_eq!(
            serde_json::to_string(&AgentPhase::RunningTool).unwrap(),
            r#""running_tool""#
        );
        assert_eq!(
            serde_json::to_string(&PermissionDecision::AllowAlways).unwrap(),
            r#""allow_always""#
        );
    }
}
