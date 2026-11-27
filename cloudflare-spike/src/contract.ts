// GENERATED from contract.schema.json, which src/contract.rs emits. Do not edit; change src/contract.rs, then run
// UPDATE_SCHEMA=1 cargo test --no-default-features --features contract-schema contract_schema_is_current && npm run gen:contract

export type ActorKind = "human" | "agent" | "system" | "service";
/**
 * Typed payload. Internally tagged on `type` (snake_case), so an
 * `exec_output` serializes as `{"type":"exec_output","stream":"stdout",...}`.
 */
export type Payload =
  | (SandboxProvisioned & {
      type: "sandbox_provisioned";
    })
  | {
      type: "sandbox_ready";
    }
  | (SandboxDestroyed & {
      type: "sandbox_destroyed";
    })
  | (RunStarted & {
      type: "run_started";
    })
  | (RunFinished & {
      type: "run_finished";
    })
  | (RunFailed & {
      type: "run_failed";
    })
  | (MessageStart & {
      type: "message_start";
    })
  | (MessageDelta & {
      type: "message_delta";
    })
  | (MessageEnd & {
      type: "message_end";
    })
  | (ToolCall & {
      type: "tool_call";
    })
  | (Thinking & {
      type: "thinking";
    })
  | (Usage & {
      type: "usage";
    })
  | (PhaseChanged & {
      type: "phase_changed";
    })
  | (TodosUpdated & {
      type: "todos_updated";
    })
  | (PermissionRequested & {
      type: "permission_requested";
    })
  | (PermissionResolved & {
      type: "permission_resolved";
    })
  | (AttentionRequired & {
      type: "attention_required";
    })
  | (Input & {
      type: "input";
    })
  | (Annotation & {
      type: "annotation";
    })
  | (Checkpoint & {
      type: "checkpoint";
    })
  | (ResultReady & {
      type: "result_ready";
    })
  | (Scored & {
      type: "scored";
    })
  | (Artifact & {
      type: "artifact";
    })
  | (ExecStarted & {
      type: "exec_started";
    })
  | (ExecOutput & {
      type: "exec_output";
    })
  | (ExecExit & {
      type: "exec_exit";
    })
  | (Custom & {
      type: "custom";
    })
  | {
      type: "unknown";
    };
export type ReasoningEffort = "low" | "medium" | "high";
export type EffectiveRuntimeLimitsEvidence =
  | {
      contextWindowTokens?: number | null;
      maxOutputTokens?: number | null;
      status: "reported";
      supportedReasoningProfiles?: string[];
    }
  | {
      reason: EvidenceUnavailableReason;
      status: "unavailable";
    };
export type EvidenceUnavailableReason = "not_reported" | "unsupported" | "redacted";
export type ServedRunProfileEvidence =
  | {
      model: string;
      profile?: string | null;
      provider?: string | null;
      reasoningProfile?: string | null;
      status: "reported";
    }
  | {
      reason: EvidenceUnavailableReason;
      status: "unavailable";
    };
export type Role = "unspecified" | "assistant" | "user" | "system";
export type ToolStatus = "unspecified" | "running" | "completed" | "error";
export type UsageSource = "unspecified" | "wire" | "native";
export type AgentPhase = "unspecified" | "queued" | "thinking" | "editing" | "running_tool" | "waiting_input" | "done";
export type TodoStatus = "unspecified" | "pending" | "in_progress" | "completed";
export type PermissionDecision = "unspecified" | "allow" | "allow_always" | "deny";
export type AttentionReason = "unspecified" | "needs_input" | "error_stalled" | "permission";
/**
 * Where the steer goes: the agent's prompt channel, the raw PTY, or a one-off exec.
 */
export type InputTarget = "agent" | "pty" | "exec";
export type StdStream = "unspecified" | "stdout" | "stderr";

/**
 * One event on a session's durable log. Envelope + a typed payload.
 *
 * `session_id` is the **partition key** — the durable identity that outlives
 * the sandboxes/runs/execs a session spans (see docs/session-event-log.md).
 * `sandbox_id`/`run_id`/`exec_id` demote to optional *correlation*: which
 * sandbox/run/exec a given line happened in.
 *
 * `seq` is monotonic **per session** and assigned by the session log on
 * append (the log is the seq authority), to DURABLE events only; ephemeral
 * telemetry carries `seq == 0` and is excluded from replay. Producers build
 * events via [`Event::session`] with `seq == 0` and let
 * [`crate::events::log::SessionLog::append`] stamp it.
 */
export interface Event {
  /**
   * Who produced this event — stamped by the producer/gateway, never
   * self-reported by the in-sandbox agent (the trust boundary; authz keys off
   * `actor`). `None` on legacy/unattributed events. See [`Actor`].
   */
  actor?: Actor | null;
  /**
   * RFC3339 timestamp.
   */
  at: string;
  ephemeral?: boolean;
  execId?: string;
  payload: Payload;
  runId?: string;
  sandboxId?: string;
  seq: number;
  sessionId?: string;
}
/**
 * Who produced an event. **Stamped by the producer/gateway from an authenticated
 * source, never self-reported by the in-sandbox agent** — unlike the old
 * `host`/`sandbox` emitter tag, authz (who may drive / approve / join) keys off
 * `actor`, so it is the trust boundary. See docs/session-event-log.md §Actor model.
 */
export interface Actor {
  display?: string;
  /**
   * Stable, kind-prefixed id (`a:<agent>`, `u:<user>`, `svc:<service>`,
   * `pillbox`) — the [`Actor`] constructors apply the prefix.
   */
  id: string;
  kind: ActorKind;
}
export interface SandboxProvisioned {
  image: string;
}
export interface SandboxDestroyed {
  reason: string;
}
export interface RunStarted {
  agent: string;
  baseSnapshot?: string;
  parentRunId?: string;
  requested?: RequestedRunProfile | null;
}
export interface RequestedRunProfile {
  model: string;
  profile?: string | null;
  provider: string;
  reasoningEffort?: ReasoningEffort | null;
}
export interface RunFinished {
  effectiveLimits?: EffectiveRuntimeLimitsEvidence | null;
  exitCode: number;
  resultSnapshot?: string;
  servedModel?: ServedRunProfileEvidence | null;
}
export interface RunFailed {
  exitCode: number;
  reason: string;
}
export interface MessageStart {
  messageId: string;
  role: Role;
}
export interface MessageDelta {
  messageId: string;
  text: string;
}
export interface MessageEnd {
  messageId: string;
  /**
   * The model that served the message; empty when unknown (e.g. a user
   * turn or a harness that doesn't record it).
   */
  model?: string;
  /**
   * Why the message stopped (`end_turn`, `tool_use`, …); empty when unknown.
   */
  stopReason?: string;
}
export interface ToolCall {
  input?: unknown;
  name: string;
  output?: string;
  status: ToolStatus;
  title?: string;
  toolCallId: string;
}
/**
 * Reasoning/thinking content the harness surfaced for a turn. First-class
 * semantic output (the transcript exposes it discretely), `content`-class and
 * local-only. Distinct from the MITM raw thinking body (blob-stored).
 */
export interface Thinking {
  text: string;
}
/**
 * Token accounting for a turn, correlated to its message by `message_id`.
 * `source` distinguishes the harness's persisted counts ([`UsageSource::Native`])
 * from MITM wire-observed counts ([`UsageSource::Wire`]) so a consumer can
 * dedupe across producers. Token fields are `Option` so "0 tokens" is
 * distinguishable from "not reported".
 */
export interface Usage {
  cacheCreationInputTokens?: number | null;
  cacheReadInputTokens?: number | null;
  costUsd?: number | null;
  inputTokens?: number | null;
  messageId: string;
  outputTokens?: number | null;
  source: UsageSource;
}
export interface PhaseChanged {
  phase: AgentPhase;
}
export interface TodosUpdated {
  todos: Todo[];
}
export interface Todo {
  status: TodoStatus;
  text: string;
}
export interface PermissionRequested {
  description?: string;
  input?: unknown;
  permissionId: string;
  tool: string;
}
export interface PermissionResolved {
  decision: PermissionDecision;
  permissionId: string;
}
export interface AttentionRequired {
  message: string;
  reason: AttentionReason;
}
/**
 * A durable, attributed steer — the §0 record of `session send` (and the managed
 * tier's `/input`). It is by definition a discrete *turn* (a submitted
 * prompt/command), distinct from the live, ephemeral PTY keystroke
 * (`Frame::Input`): this persists + replays + carries `actor`, so a late joiner
 * sees who drove the agent and with what. (`data` for binary input is a future
 * addition; `session send` is text today.)
 */
export interface Input {
  target: InputTarget;
  text: string;
}
/**
 * An attributed, durable comment on a session that does NOT drive the agent —
 * the async, keyboard-free "chime in" (Slack-thread style). Unlike [`Input`] it
 * carries no driver semantics (anyone may annotate, no arbitration), so it's how
 * a non-driver participates; an orchestrator may optionally inject it as agent
 * context. `anchor` is a free-form reference to what it's about (a seq, a path, a
 * message id).
 */
export interface Annotation {
  anchor?: string;
  text: string;
}
export interface Checkpoint {
  message?: string;
  snapshotHandle: string;
}
export interface ResultReady {
  snapshotHandle: string;
}
/**
 * An **external, verifiable** grade of a session's result — produced by running
 * a grader against the result-snapshot (`pillbox session score --cmd …`), NOT
 * self-reported by the agent. (`RunFinished` / `session done --status` is
 * self-stamped → Goodhart-banned as a reward.) This is the substrate primitive
 * the optimization loops gate on: GEPA needs a coarse verifiable score, and the
 * `feedback` carries the textual gradient (test output, stderr, diff) that does
 * the actual optimizing — not just the scalar.
 */
export interface Scored {
  /**
   * Per-criterion verdicts from a `--rubric` grade — the rich,
   * decomposed feedback an optimizer reflects on (which criterion failed and
   * why), vs the single `feedback` blob. Empty for a plain `--cmd` grade.
   */
  criteria?: Criterion[];
  /**
   * The grader's captured output (the gradient, not just the scalar). For a
   * rubric grade, a rendered per-criterion summary; `criteria` carries the
   * structured detail.
   */
  feedback: string;
  /**
   * What produced the grade — the verifier command line (or a grader id).
   */
  grader: string;
  /**
   * Verifiable pass/fail = the grader's exit status (0 → `true`).
   */
  passed: boolean;
  /**
   * Normalized score in `[0,1]`. A plain `--cmd` grade is binary
   * (`passed` → 1.0, else 0.0); a `--rubric` grade is the fraction of
   * criteria that passed — a real gradient, not just the scalar.
   */
  score: number;
}
/**
 * One rubric criterion's verdict: a named, independently-verifiable check
 * (its own command exit) plus its captured output. The structured unit a
 * `--rubric` grade decomposes into.
 */
export interface Criterion {
  /**
   * The criterion command's combined output, tail-capped. Empty when it
   * produced none (e.g. a silent passing check).
   */
  feedback?: string;
  name: string;
  /**
   * The criterion's command exited 0.
   */
  passed: boolean;
}
/**
 * A **structured session artifact** — a typed, durable output that is NOT an
 * ordinary agent message: a grader report, a judge critique, a dispatch
 * worker summary, a code-exploration citation set, a self-harness proposal,
 * patch metadata. The log line stays small (kind + summary + a content-
 * addressed blob ref); the payload body lives in the session's blob store
 * (`sessions/<id>/blobs/<sha256>`), dereferenced lazily — large tool output
 * never inlines into the spine. The enabling primitive for the eval loop, the
 * dispatch evidence channel (#73), and any host-side tool (a FastContext
 * explorer, a grader) that wants to attach output without overloading the
 * transcript. See docs/session-event-log.md §Payload taxonomy / §Blob store.
 */
export interface Artifact {
  /**
   * The body, by reference: a content-addressed blob handle (sha256 hex) in
   * the session's blob store. The large payload never inlines into the log.
   */
  blobRef?: string;
  /**
   * Size of the referenced blob in bytes (0 if none).
   */
  bytes?: number;
  /**
   * Poolability class (docs/session-event-log.md §Content vs signal):
   * `content` is local-only (raw code/output/critique); `signal` is the
   * scrub-poolable metadata (scores, counts, names). Defaults to `content`
   * — the safe default, so a body never egresses unless a producer asserts
   * it is poolable signal.
   */
  class?: "content" | "signal";
  /**
   * MIME-ish content type of the body (`application/json`, `text/plain`,
   * `application/x-ndjson`, …). Empty = opaque bytes.
   */
  contentType?: string;
  /**
   * Typed kind in a dotted namespace: `eval.grader_report`, `judge.report`,
   * `dispatch.worker_summary`, `code_explore.citations`,
   * `self_harness.proposal`, `patch.summary`, … A free-form string (not an
   * enum) so a new producer adds a kind without a contract bump; readers
   * filter by prefix.
   */
  kind: string;
  /**
   * One-line summary — enough to triage from the log without dereferencing
   * the blob.
   */
  summary?: string;
  /**
   * Worker correlation for fan-out artifacts (a dispatch worker id), so a
   * reader can group a run's per-worker summaries. Empty when not applicable.
   */
  workerId?: string;
}
export interface ExecStarted {
  argv: string[];
}
/**
 * `data` is base64-encoded bytes (matches the proto `bytes` field under
 * protobuf-JSON) so binary stdout survives the wire intact.
 */
export interface ExecOutput {
  data: string;
  stream: StdStream;
}
export interface ExecExit {
  code: number;
}
export interface Custom {
  name: string;
  payload?: unknown;
}
