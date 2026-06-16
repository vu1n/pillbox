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
//! `pillbox sandbox` runtime (`commands::sandbox`) is the per-emitter producer
//! today; the per-session [`crate::events::log::SessionLog`] is the durable
//! spine new producers target (see docs/session-event-log.md).

// Contract surface lands ahead of its first producer (contract-first).
#![allow(dead_code)]

use std::io::Write;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One event on a session's durable log. Envelope + a typed payload.
///
/// `session_id` is the **partition key** — the durable identity that outlives
/// the sandboxes/runs/execs a session spans (see docs/session-event-log.md).
/// `sandbox_id`/`run_id`/`exec_id` demote to optional *correlation*: which
/// sandbox/run/exec a given line happened in.
///
/// `seq` is monotonic **per session** and assigned by the session log on
/// append (the log is the seq authority), to DURABLE events only; ephemeral
/// telemetry carries `seq == 0` and is excluded from replay. Producers build
/// events via [`Event::session`] with `seq == 0` and let
/// [`crate::events::log::SessionLog::append`] stamp it.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Event {
    pub(crate) seq: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) sandbox_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) exec_id: String,
    /// RFC3339 timestamp.
    pub(crate) at: String,
    /// Who produced this event — stamped by the producer/gateway, never
    /// self-reported by the in-sandbox agent (the trust boundary; authz keys off
    /// `actor`). `None` on legacy/unattributed events. See [`Actor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) actor: Option<Actor>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) ephemeral: bool,
    pub(crate) payload: Payload,
}

impl Event {
    /// Shared field defaults, timestamped now; the durable builders set only
    /// their partition/correlation key + `seq` on top.
    fn at_now(seq: u64, payload: Payload) -> Self {
        Self {
            seq,
            session_id: String::new(),
            sandbox_id: String::new(),
            run_id: String::new(),
            exec_id: String::new(),
            at: crate::session::now_rfc3339(),
            actor: None,
            ephemeral: false,
            payload,
        }
    }

    /// A durable, session-partitioned event with `seq` left at 0 for the
    /// session log to assign on append (the log is the seq authority — see
    /// docs/session-event-log.md). `session_id` is the partition key;
    /// sandbox/run/exec correlation is layered on via the `with_*` builders.
    pub(crate) fn session(session_id: impl Into<String>, payload: Payload) -> Self {
        Self {
            session_id: session_id.into(),
            ..Self::at_now(0, payload)
        }
    }

    /// Legacy builder for non-session-partitioned events: the caller's counter
    /// assigns `seq` and `session_id` stays empty. Used by the per-emitter
    /// [`EventEmitter`] path (`commands::sandbox`); new session-log producers
    /// use [`Event::session`] and let the log assign `seq`.
    pub(crate) fn durable(seq: u64, sandbox_id: impl Into<String>, payload: Payload) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            ..Self::at_now(seq, payload)
        }
    }

    pub(crate) fn with_sandbox(mut self, sandbox_id: impl Into<String>) -> Self {
        self.sandbox_id = sandbox_id.into();
        self
    }

    pub(crate) fn with_run(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = run_id.into();
        self
    }

    pub(crate) fn with_exec(mut self, exec_id: impl Into<String>) -> Self {
        self.exec_id = exec_id.into();
        self
    }

    pub(crate) fn with_actor(mut self, actor: Actor) -> Self {
        self.actor = Some(actor);
        self
    }
}

/// Who produced an event. **Stamped by the producer/gateway from an authenticated
/// source, never self-reported by the in-sandbox agent** — unlike the old
/// `host`/`sandbox` emitter tag, authz (who may drive / approve / join) keys off
/// `actor`, so it is the trust boundary. See docs/session-event-log.md §Actor model.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Actor {
    pub(crate) kind: ActorKind,
    /// Stable, kind-prefixed id (`a:<agent>`, `u:<user>`, `svc:<service>`,
    /// `pillbox`) — the [`Actor`] constructors apply the prefix.
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) display: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActorKind {
    Human,
    Agent,
    System,
    Service,
}

impl Actor {
    /// The coding agent's own output (`message_*`, `tool_call`, …).
    pub(crate) fn agent(id: impl AsRef<str>) -> Self {
        Self::new(ActorKind::Agent, format!("a:{}", id.as_ref()))
    }
    /// pillbox itself — lifecycle, sequencing, snapshots.
    pub(crate) fn system() -> Self {
        Self::new(ActorKind::System, "pillbox".into())
    }
    /// A person — input, annotations, approvals.
    pub(crate) fn human(id: impl AsRef<str>) -> Self {
        Self::new(ActorKind::Human, format!("u:{}", id.as_ref()))
    }
    /// A non-human automated participant — a grader, CI, an orchestrator.
    pub(crate) fn service(id: impl AsRef<str>) -> Self {
        Self::new(ActorKind::Service, format!("svc:{}", id.as_ref()))
    }
    fn new(kind: ActorKind, id: String) -> Self {
        Self {
            kind,
            id,
            display: String::new(),
        }
    }
}

/// Typed payload. Internally tagged on `type` (snake_case), so an
/// `exec_output` serializes as `{"type":"exec_output","stream":"stdout",...}`.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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
    Thinking(Thinking),
    Usage(Usage),
    // ephemeral card telemetry
    PhaseChanged(PhaseChanged),
    TodosUpdated(TodosUpdated),
    // human-in-the-loop
    PermissionRequested(PermissionRequested),
    PermissionResolved(PermissionResolved),
    AttentionRequired(AttentionRequired),
    // multiplayer: the durable, attributed steer (distinct from the live,
    // ephemeral PTY keystroke frame) — `session send` records one per drive.
    Input(Input),
    // multiplayer: an attributed comment that does NOT drive the agent (the
    // async "chime in"; `session annotate`) — orchestrators may inject it as context.
    Annotation(Annotation),
    // workspace
    Checkpoint(Checkpoint),
    ResultReady(ResultReady),
    // reward (external, verifiable grade — see `Scored`)
    Scored(Scored),
    // structured session output that is NOT an ordinary agent message — a
    // grader report, judge critique, dispatch worker summary, code-exploration
    // citations, … The body lives in the blob store; this records the typed ref.
    Artifact(Artifact),
    // exec channel
    ExecStarted(ExecStarted),
    ExecOutput(ExecOutput),
    ExecExit(ExecExit),
    // extension valve
    Custom(Custom),
    /// Forward/foreign-compat catch-all: any payload `type` this binary
    /// doesn't know deserializes here instead of failing the whole line, so a
    /// newer or foreign producer can't break replay/decode of the rest of the
    /// log (see docs/session-event-log.md §Versioning). Unit + `#[serde(other)]`
    /// is the only shape serde permits for an internally-tagged catch-all, so
    /// the original tag + body are *not* preserved on round-trip yet (it
    /// re-serializes as `{"type":"unknown"}`); body-preserving Unknown is the
    /// upgrade that lands with foreign-trace re-export.
    #[serde(other)]
    Unknown,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxProvisioned {
    pub(crate) image: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SandboxDestroyed {
    pub(crate) reason: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunStarted {
    pub(crate) agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) parent_run_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) base_snapshot: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunFinished {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) result_snapshot: String,
    pub(crate) exit_code: i32,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunFailed {
    pub(crate) reason: String,
    pub(crate) exit_code: i32,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageStart {
    pub(crate) message_id: String,
    pub(crate) role: Role,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageDelta {
    pub(crate) message_id: String,
    pub(crate) text: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageEnd {
    pub(crate) message_id: String,
    /// The model that served the message; empty when unknown (e.g. a user
    /// turn or a harness that doesn't record it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) model: String,
    /// Why the message stopped (`end_turn`, `tool_use`, …); empty when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) stop_reason: String,
}

impl MessageEnd {
    /// End a message with no metadata — the common case for the live stream
    /// parsers, whose wire format doesn't carry model/stop_reason at end-of-
    /// message (only the post-hoc transcript does, and it builds the full
    /// struct directly).
    pub(crate) fn new(message_id: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            model: String::new(),
            stop_reason: String::new(),
        }
    }
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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

/// Reasoning/thinking content the harness surfaced for a turn. First-class
/// semantic output (the transcript exposes it discretely), `content`-class and
/// local-only. Distinct from the MITM raw thinking body (blob-stored).
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Thinking {
    pub(crate) text: String,
}

/// Token accounting for a turn, correlated to its message by `message_id`.
/// `source` distinguishes the harness's persisted counts ([`UsageSource::Native`])
/// from MITM wire-observed counts ([`UsageSource::Wire`]) so a consumer can
/// dedupe across producers. Token fields are `Option` so "0 tokens" is
/// distinguishable from "not reported".
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Usage {
    pub(crate) message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_creation_input_tokens: Option<u64>,
    pub(crate) source: UsageSource,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhaseChanged {
    pub(crate) phase: AgentPhase,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Todo {
    pub(crate) text: String,
    pub(crate) status: TodoStatus,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TodosUpdated {
    pub(crate) todos: Vec<Todo>,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionResolved {
    pub(crate) permission_id: String,
    pub(crate) decision: PermissionDecision,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttentionRequired {
    pub(crate) reason: AttentionReason,
    pub(crate) message: String,
}

/// A durable, attributed steer — the §0 record of `session send` (and the managed
/// tier's `/input`). It is by definition a discrete *turn* (a submitted
/// prompt/command), distinct from the live, ephemeral PTY keystroke
/// (`Frame::Input`): this persists + replays + carries `actor`, so a late joiner
/// sees who drove the agent and with what. (`data` for binary input is a future
/// addition; `session send` is text today.)
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Input {
    pub(crate) text: String,
    pub(crate) target: InputTarget,
}

/// Where the steer goes: the agent's prompt channel, the raw PTY, or a one-off exec.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InputTarget {
    Agent,
    Pty,
    Exec,
}

/// An attributed, durable comment on a session that does NOT drive the agent —
/// the async, keyboard-free "chime in" (Slack-thread style). Unlike [`Input`] it
/// carries no driver semantics (anyone may annotate, no arbitration), so it's how
/// a non-driver participates; an orchestrator may optionally inject it as agent
/// context. `anchor` is a free-form reference to what it's about (a seq, a path, a
/// message id).
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Annotation {
    pub(crate) text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) anchor: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Checkpoint {
    pub(crate) snapshot_handle: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) message: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResultReady {
    pub(crate) snapshot_handle: String,
}

/// An **external, verifiable** grade of a session's result — produced by running
/// a grader against the result-snapshot (`pillbox session score --cmd …`), NOT
/// self-reported by the agent. (`RunFinished` / `session done --status` is
/// self-stamped → Goodhart-banned as a reward.) This is the substrate primitive
/// the optimization loops gate on: GEPA needs a coarse verifiable score, and the
/// `feedback` carries the textual gradient (test output, stderr, diff) that does
/// the actual optimizing — not just the scalar.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Scored {
    /// What produced the grade — the verifier command line (or a grader id).
    pub(crate) grader: String,
    /// Verifiable pass/fail = the grader's exit status (0 → `true`).
    pub(crate) passed: bool,
    /// Normalized score in `[0,1]`. A plain `--cmd` grade is binary
    /// (`passed` → 1.0, else 0.0); a `--rubric` grade is the fraction of
    /// criteria that passed — a real gradient, not just the scalar.
    pub(crate) score: f64,
    /// The grader's captured output (the gradient, not just the scalar). For a
    /// rubric grade, a rendered per-criterion summary; `criteria` carries the
    /// structured detail.
    pub(crate) feedback: String,
    /// Per-criterion verdicts from a `--rubric` grade — the rich,
    /// decomposed feedback an optimizer reflects on (which criterion failed and
    /// why), vs the single `feedback` blob. Empty for a plain `--cmd` grade.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) criteria: Vec<Criterion>,
}

/// One rubric criterion's verdict: a named, independently-verifiable check
/// (its own command exit) plus its captured output. The structured unit a
/// `--rubric` grade decomposes into.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Criterion {
    pub(crate) name: String,
    /// The criterion's command exited 0.
    pub(crate) passed: bool,
    /// The criterion command's combined output, tail-capped. Empty when it
    /// produced none (e.g. a silent passing check).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) feedback: String,
}

/// A **structured session artifact** — a typed, durable output that is NOT an
/// ordinary agent message: a grader report, a judge critique, a dispatch
/// worker summary, a code-exploration citation set, a self-harness proposal,
/// patch metadata. The log line stays small (kind + summary + a content-
/// addressed blob ref); the payload body lives in the session's blob store
/// (`sessions/<id>/blobs/<sha256>`), dereferenced lazily — large tool output
/// never inlines into the spine. The enabling primitive for the eval loop, the
/// dispatch evidence channel (#73), and any host-side tool (a FastContext
/// explorer, a grader) that wants to attach output without overloading the
/// transcript. See docs/session-event-log.md §Payload taxonomy / §Blob store.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Artifact {
    /// Typed kind in a dotted namespace: `eval.grader_report`, `judge.report`,
    /// `dispatch.worker_summary`, `code_explore.citations`,
    /// `self_harness.proposal`, `patch.summary`, … A free-form string (not an
    /// enum) so a new producer adds a kind without a contract bump; readers
    /// filter by prefix.
    pub(crate) kind: String,
    /// One-line summary — enough to triage from the log without dereferencing
    /// the blob.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) summary: String,
    /// MIME-ish content type of the body (`application/json`, `text/plain`,
    /// `application/x-ndjson`, …). Empty = opaque bytes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) content_type: String,
    /// Poolability class (docs/session-event-log.md §Content vs signal):
    /// `content` is local-only (raw code/output/critique); `signal` is the
    /// scrub-poolable metadata (scores, counts, names). Defaults to `content`
    /// — the safe default, so a body never egresses unless a producer asserts
    /// it is poolable signal.
    #[serde(default)]
    pub(crate) class: ArtifactClass,
    /// The body, by reference: a content-addressed blob handle (sha256 hex) in
    /// the session's blob store. The large payload never inlines into the log.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) blob_ref: String,
    /// Size of the referenced blob in bytes (0 if none).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub(crate) bytes: u64,
    /// Worker correlation for fan-out artifacts (a dispatch worker id), so a
    /// reader can group a run's per-worker summaries. Empty when not applicable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) worker_id: String,
}

/// The content-vs-signal poolability split (docs/session-event-log.md): a
/// structural gate so "pool the metadata, not the code" is enforced by the
/// schema, not by remembering to redact. `Content` is the safe default.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactClass {
    /// Local-only: raw code, prompts, messages, tool I/O, critiques. Never egresses.
    #[default]
    Content,
    /// Poolable after scrub: scores, exit codes, pass/fail, counts, names.
    Signal,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecStarted {
    pub(crate) argv: Vec<String>,
}

/// `data` is base64-encoded bytes (matches the proto `bytes` field under
/// protobuf-JSON) so binary stdout survives the wire intact.
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecOutput {
    pub(crate) stream: StdStream,
    pub(crate) data: String,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecExit {
    pub(crate) code: i32,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Custom {
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<Value>,
}

// ── Enums (snake_case on the wire; `Unspecified` is the deserialize fallback) ──

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Unspecified,
    Assistant,
    User,
    System,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolStatus {
    Unspecified,
    Running,
    Completed,
    Error,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UsageSource {
    Unspecified,
    /// MITM wire-observed.
    Wire,
    /// Harness-persisted (transcript).
    Native,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TodoStatus {
    Unspecified,
    Pending,
    InProgress,
    Completed,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StdStream {
    Unspecified,
    Stdout,
    Stderr,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttentionReason {
    Unspecified,
    NeedsInput,
    ErrorStalled,
    Permission,
}

#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
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
    fn actor_stamps_kind_and_prefixed_id_and_round_trips() {
        let ev = Event::session("s", Payload::SandboxReady).with_actor(Actor::agent("claude"));
        let s = serde_json::to_string(&ev).unwrap();
        assert!(
            s.contains(r#""actor":{"kind":"agent","id":"a:claude"}"#),
            "{s}"
        );
        assert_eq!(reparse(&ev), ev);
        // Each constructor: right kind + kind-prefixed id.
        assert_eq!(Actor::system().id, "pillbox");
        assert_eq!(Actor::system().kind, ActorKind::System);
        assert_eq!(Actor::human("alice").id, "u:alice");
        assert_eq!(Actor::service("grader").id, "svc:grader");
    }

    #[test]
    fn input_payload_round_trips_with_human_actor() {
        let ev = Event::session(
            "s",
            Payload::Input(Input {
                text: "fix the bug".into(),
                target: InputTarget::Agent,
            }),
        )
        .with_actor(Actor::human("alice"));
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"input""#), "{s}");
        assert!(s.contains(r#""target":"agent""#), "{s}");
        assert!(
            s.contains(r#""actor":{"kind":"human","id":"u:alice"}"#),
            "{s}"
        );
        assert_eq!(reparse(&ev), ev);
    }

    #[test]
    fn annotation_round_trips_and_omits_empty_anchor() {
        let ev = Event::session(
            "s",
            Payload::Annotation(Annotation {
                text: "lgtm, but check the empty case".into(),
                anchor: "path/to/x.rs:42".into(),
            }),
        )
        .with_actor(Actor::human("bob"));
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"annotation""#), "{s}");
        assert!(s.contains(r#""anchor":"path/to/x.rs:42""#), "{s}");
        assert!(
            s.contains(r#""actor":{"kind":"human","id":"u:bob"}"#),
            "{s}"
        );
        assert_eq!(reparse(&ev), ev);
        // anchor omitted from the wire when empty.
        let bare = serde_json::to_string(&Event::session(
            "s",
            Payload::Annotation(Annotation {
                text: "hi".into(),
                anchor: String::new(),
            }),
        ))
        .unwrap();
        assert!(!bare.contains("anchor"), "{bare}");
    }

    #[test]
    fn legacy_event_without_actor_parses_as_none() {
        // Forward/backward-compat: a pre-actor log line (no `actor` field) decodes
        // to `actor: None`, not an error.
        let line = r#"{"seq":1,"sessionId":"s","at":"t","payload":{"type":"sandbox_ready"}}"#;
        let ev: Event = serde_json::from_str(line).unwrap();
        assert_eq!(ev.actor, None);
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
    fn scored_serializes_as_the_reward_contract() {
        let ev = Event::session(
            "s",
            Payload::Scored(Scored {
                grader: "pytest -q".into(),
                passed: false,
                score: 0.0,
                feedback: "1 failed, 3 passed".into(),
                criteria: Vec::new(),
            }),
        );
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"scored""#), "{s}");
        assert!(s.contains(r#""grader":"pytest -q""#), "{s}");
        assert!(s.contains(r#""passed":false"#), "{s}");
        assert!(s.contains(r#""score":0.0"#), "{s}");
        assert!(s.contains(r#""feedback":"1 failed, 3 passed""#), "{s}");
        // Empty criteria are omitted — the plain --cmd reward shape is unchanged.
        assert!(!s.contains("criteria"), "{s}");
        assert_eq!(reparse(&ev), ev);
    }

    #[test]
    fn artifact_serializes_with_blob_ref_and_defaults() {
        let ev = Event::session(
            "s",
            Payload::Artifact(Artifact {
                kind: "dispatch.worker_summary".into(),
                summary: "worker w2 passed 4/4".into(),
                content_type: "application/json".into(),
                class: ArtifactClass::Signal,
                blob_ref: "abc123".into(),
                bytes: 512,
                worker_id: "w2".into(),
            }),
        );
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""type":"artifact""#), "{s}");
        assert!(s.contains(r#""kind":"dispatch.worker_summary""#), "{s}");
        assert!(s.contains(r#""class":"signal""#), "{s}");
        assert!(s.contains(r#""blobRef":"abc123""#), "{s}");
        assert!(s.contains(r#""bytes":512"#), "{s}");
        assert!(s.contains(r#""workerId":"w2""#), "{s}");
        assert_eq!(reparse(&ev), ev);
    }

    #[test]
    fn artifact_omits_empty_fields_and_defaults_class_to_content() {
        // A minimal artifact (just a kind + blob ref) drops every empty field;
        // `class` defaults to the safe `content` on the way back in.
        let minimal: Artifact = serde_json::from_str(r#"{"kind":"judge.report"}"#).unwrap();
        assert_eq!(minimal.class, ArtifactClass::Content);
        assert!(minimal.summary.is_empty() && minimal.blob_ref.is_empty());
        assert_eq!(minimal.bytes, 0);

        let ev = Event::session("s", Payload::Artifact(minimal));
        let s = serde_json::to_string(&ev).unwrap();
        // Defaulted/empty fields omitted; class is always present (the poolability gate).
        assert!(
            !s.contains("summary") && !s.contains("blobRef") && !s.contains("bytes"),
            "{s}"
        );
        assert!(s.contains(r#""class":"content""#), "{s}");
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
    fn session_event_carries_partition_key_and_unassigned_seq() {
        // The session-partitioned builder: seq stays 0 (the log assigns it),
        // session_id is the partition key, sandbox/run/exec are absent until
        // correlated. Round-trips with sandbox_id omitted from the wire.
        let ev = Event::session(
            "sess-abc",
            Payload::ToolCall(ToolCall {
                tool_call_id: "tc-1".into(),
                name: "Bash".into(),
                status: ToolStatus::Running,
                input: None,
                output: String::new(),
                title: String::new(),
            }),
        );
        assert_eq!(ev.seq, 0, "log assigns seq, not the producer");
        assert_eq!(ev.session_id, "sess-abc");
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains(r#""sessionId":"sess-abc""#), "{s}");
        assert!(!s.contains("sandboxId"), "empty correlation omitted: {s}");
        assert_eq!(reparse(&ev), ev);

        // Correlation layers on without disturbing the partition key.
        let correlated = ev.clone().with_sandbox("sb-1").with_run("run-1");
        assert_eq!(correlated.session_id, "sess-abc");
        assert_eq!(correlated.sandbox_id, "sb-1");
        assert_eq!(reparse(&correlated), correlated);
    }

    #[test]
    fn unknown_payload_type_decodes_to_unknown_not_error() {
        // A newer/foreign producer emits a payload type this binary doesn't
        // know. It must decode (to Unknown) rather than fail the whole event —
        // the forward-compat guarantee the durable log relies on for replay.
        let line = r#"{"seq":5,"sessionId":"s","at":"2026-05-31T00:00:00Z","payload":{"type":"some_future_event","detail":"x"}}"#;
        let ev: Event = serde_json::from_str(line).expect("unknown type must not break decode");
        assert_eq!(ev.seq, 5);
        assert!(matches!(ev.payload, Payload::Unknown));
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

/// Cross-language contract sync gate. `contract.rs` is canonical; the committed
/// JSON Schema (`cloudflare-spike/contract.schema.json`) is generated from it,
/// and the TS contract is generated from that schema. This test fails if the
/// schema is stale, so a contract change that isn't propagated can't merge.
/// Regenerate after a deliberate change:
///   `UPDATE_SCHEMA=1 cargo test --features contract-schema contract_schema_is_current`
#[cfg(all(test, feature = "contract-schema"))]
mod schema_gate {
    /// The canonical schema's path, relative to the crate root.
    const SCHEMA_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/cloudflare-spike/contract.schema.json"
    );

    fn rendered_schema() -> String {
        // `Event` is the root; schemars walks the whole reachable graph (Payload +
        // every variant struct + the enums), honoring the serde attrs (camelCase,
        // the internally-tagged `type` discriminant, default/skip → optional).
        let schema = schemars::schema_for!(super::Event);
        let mut json = serde_json::to_string_pretty(&schema).expect("serialize schema");
        json.push('\n');
        json
    }

    #[test]
    fn contract_schema_is_current() {
        let rendered = rendered_schema();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(SCHEMA_PATH, &rendered).expect("write contract.schema.json");
            return;
        }
        let committed = std::fs::read_to_string(SCHEMA_PATH).unwrap_or_default();
        assert_eq!(
            committed, rendered,
            "§0 JSON Schema is stale — contract.rs changed without regenerating. Run: \
             UPDATE_SCHEMA=1 cargo test --features contract-schema contract_schema_is_current \
             (then regenerate the TS: `npm run gen:contract` in cloudflare-spike/)"
        );
    }
}
