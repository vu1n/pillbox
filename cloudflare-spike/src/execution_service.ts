import type { getSandbox } from "@cloudflare/sandbox";
import {
  computeExecutionIdentityDigest,
  computeInvocationRequestHash,
  type CancelInvocationV2Request,
  type ExecuteInvocationV2ErrorCode,
  type ExecuteInvocationV2Request,
  type ExecuteInvocationV2Result,
  type CompletedExecutionResultSessionRef,
  type ExecutionAttribution,
  type ExecutionEvidencePage,
  type ExecutionResultSessionRef,
  type GetInvocationV2Request,
  type JsonValue,
  MAX_EVIDENCE_PAGE_SIZE,
  validateCancelInvocationV2Request,
  validateExecuteInvocationV2Request,
  validateGetInvocationV2Request,
} from "./codex_execution.js";
import type {
  ExecutionArtifact,
  ExecutionArtifactStore,
} from "./execution_artifacts.js";
import {
  ExecutionArtifactConflictError,
  MAX_EXECUTION_ARTIFACT_BYTES,
  MAX_EXECUTION_EVIDENCE_EVENT_BYTES,
} from "./execution_artifacts.js";
import type {
  ExecutionClaim,
  ExecutionClaimInput,
  ExecutionRecord,
  ExecutionStore,
} from "./execution_store.js";
import type { ManagedExecutionOwner } from "./managed_ownership.js";
import {
  ManagedExecutionAllowanceError,
  ManagedReservationAccessError,
  type ManagedExecutionAllowance,
  type ManagedExecutionReservationStore,
} from "./managed_reservation.js";
import { safeHuddlesRuntimeDiagnostic } from "./huddles_policy.js";
import {
  ManagedAdmissionError,
  managedAdmissionPolicy,
  requireManagedAdmission,
  type ManagedAdmissionPolicy,
} from "./managed_admission.js";
import { driveOpencodeTurn } from "./opencode_turn.js";
import type { Payload } from "./contract.js";
import {
  type RunCostAnalytics,
  type RunCostEnvelope,
  type RunCostMeter,
  sealArtifactCostBytes,
} from "./run_cost.js";

export const EXECUTION_OWNER_LEASE_MS = 10 * 60 * 1_000;

const MISSING_POSITIONAL_EVIDENCE_ERROR = {
  code: "runtime_failed",
  message: "Pillbox managed invocation completed without immutable positional evidence",
} as const;

type ExecutionTerminal =
  | {
      readonly status: "completed";
      readonly output: { readonly text?: string; readonly json?: JsonValue };
    }
  | {
      readonly status: "failed" | "cancelled" | "interrupted";
      readonly error: {
        readonly code: ExecuteInvocationV2ErrorCode;
        readonly message: string;
      };
    };

export interface RuntimeTurnResult {
  readonly served_model: string | null;
  readonly output?: { readonly text?: string; readonly json?: JsonValue };
  readonly error?: {
    readonly code: ExecuteInvocationV2ErrorCode;
    readonly message: string;
  };
  readonly evidence: readonly JsonValue[];
}

export interface ExecutionRuntime {
  execute(request: ExecuteInvocationV2Request): Promise<RuntimeTurnResult>;
  cancel(request: CancelInvocationV2Request, session_id: string): Promise<void>;
}

export type ExecutionOperationAuthorization =
  | {
      readonly operation: "execute";
      readonly request: ExecuteInvocationV2Request;
    }
  | {
      readonly operation: "status";
      readonly request: GetInvocationV2Request;
    }
  | {
      readonly operation: "cancel";
      readonly request: CancelInvocationV2Request;
    };

/** Pre-access authorization returns only the opaque durable owner identity. */
export type ExecutionOperationAuthorizer = (
  input: ExecutionOperationAuthorization,
) => Promise<ManagedExecutionOwner>;

export interface ExecutionServiceOptions {
  readonly now?: () => number;
  readonly ownerToken?: () => string;
  readonly costMeter?: RunCostMeter;
  readonly analytics?: RunCostAnalytics;
  readonly sandboxProfile?: string;
  readonly admission?: ManagedAdmissionPolicy;
  readonly allowance?: ManagedExecutionAllowance | null;
  readonly reservations: ManagedExecutionReservationStore;
  readonly authorizer: ExecutionOperationAuthorizer;
}

export class ExecutionNotFoundError extends Error {
  readonly code = "execution_not_found" as const;

  constructor(invocation_id: string) {
    super(`execution '${invocation_id}' was not found`);
    this.name = "ExecutionNotFoundError";
  }
}

export class ExecutionService {
  private readonly store: ExecutionStore;
  private readonly artifacts: ExecutionArtifactStore;
  private readonly runtime: ExecutionRuntime;
  private readonly now: () => number;
  private readonly ownerToken: () => string;
  private readonly costMeter: RunCostMeter | undefined;
  private readonly analytics: RunCostAnalytics | undefined;
  private readonly sandboxProfile: string | null;
  private readonly admission: ManagedAdmissionPolicy;
  private readonly allowance: ManagedExecutionAllowance | null;
  private readonly reservations: ManagedExecutionReservationStore;
  private readonly authorizer: ExecutionOperationAuthorizer;

  constructor(
    store: ExecutionStore,
    artifacts: ExecutionArtifactStore,
    runtime: ExecutionRuntime,
    options: ExecutionServiceOptions,
  ) {
    this.store = store;
    this.artifacts = artifacts;
    this.runtime = runtime;
    this.now = options.now ?? Date.now;
    this.ownerToken = options.ownerToken ?? (() => crypto.randomUUID());
    this.costMeter = options.costMeter;
    this.analytics = options.analytics;
    this.sandboxProfile = options.sandboxProfile ?? null;
    this.admission = options.admission ?? managedAdmissionPolicy(undefined);
    this.allowance = options.allowance ?? null;
    this.reservations = options.reservations;
    this.authorizer = options.authorizer;
  }

  async executeInvocation(value: unknown): Promise<ExecuteInvocationV2Result> {
    const request = await validateExecuteInvocationV2Request(value);
    const requestHash = await computeInvocationRequestHash(request);
    const executionDigest = await computeExecutionIdentityDigest(
      request.execution,
      request.execution_policy_revision,
    );
    const owner = await this.authorizer({ operation: "execute", request });
    try {
      requireManagedAdmission(this.admission);
    } catch (cause) {
      if (!(cause instanceof ManagedAdmissionError)) throw cause;
      return this.managedDisabledResult(
        request,
        requestHash,
        executionDigest,
        cause.message,
      );
    }
    const unsupported = unsupportedManagedRequest(request);
    if (unsupported !== undefined) {
      return this.preClaimFailureResult(
        request,
        requestHash,
        executionDigest,
        unsupported,
      );
    }
    if (this.allowance === null) {
      return this.managedDisabledResult(
        request,
        requestHash,
        executionDigest,
        "Pillbox managed execution allowance is not configured",
      );
    }
    const now = this.now();
    let reservation;
    try {
      reservation = await this.reservations.claimExecution(
        {
          invocation_id: request.invocation_id,
          session_id: request.session_ref.session_id,
          owner,
          execution_request_hash: requestHash,
          now_ms: now,
        },
        this.allowance,
      );
    } catch (cause) {
      if (cause instanceof ManagedExecutionAllowanceError) {
        return this.managedDisabledResult(
          request,
          requestHash,
          executionDigest,
          cause.message,
        );
      }
      if (cause instanceof ManagedReservationAccessError) {
        throw new ExecutionNotFoundError(request.invocation_id);
      }
      throw cause;
    }
    if (reservation.kind === "conflict") {
      return this.reservationConflictResult(
        request,
        reservation.record.execution_request_hash,
        requestHash,
        executionDigest,
      );
    }
    if (reservation.record.status !== "ready") {
      throw new ExecutionNotFoundError(request.invocation_id);
    }
    const input: ExecutionClaimInput = {
      invocation_id: request.invocation_id,
      idempotency_key: request.idempotency_key,
      request_hash: requestHash,
      execution_digest: executionDigest,
      execution_policy_revision: request.execution_policy_revision,
      session_id: request.session_ref.session_id,
      owner,
      allowance_epoch: this.allowance.deployment_epoch,
      allowance_limit: this.allowance.execution_limit,
      attribution: attributionFromRequest(request, null),
      owner_token: this.ownerToken(),
      now_ms: now,
      lease_expires_at_ms: now + EXECUTION_OWNER_LEASE_MS,
    };
    const claim: ExecutionClaim = await this.store.claim(input);
    if (claim.kind === "unavailable") {
      throw new ExecutionNotFoundError(request.invocation_id);
    }
    if (claim.kind === "conflict") {
      return this.conflictResult(request, claim.record, requestHash);
    }
    if (claim.kind === "reused") {
      return this.resultForRecord(request, claim.record, {
        after: 0,
        limit: MAX_EVIDENCE_PAGE_SIZE,
      });
    }

    let turn: RuntimeTurnResult;
    try {
      turn = await this.runtime.execute(request);
    } catch (cause) {
      turn = {
        served_model: null,
        error: {
          code: "runtime_failed",
          message: "Pillbox managed invocation failed",
        },
        evidence: [
          {
            type: "attention_required",
            reason: "error_stalled",
            message: safeHuddlesRuntimeDiagnostic(cause),
          },
        ],
      };
    }
    return this.finishTurn(request, claim.record, turn, "created");
  }

  async getExecutionStatus(value: unknown): Promise<ExecuteInvocationV2Result> {
    const request = validateGetInvocationV2Request(value);
    const owner = await this.authorizer({ operation: "status", request });
    const record = await this.requireRecord(request.invocation_id, owner);
    return this.resultForRecord(
      undefined,
      record,
      { after: request.evidence_after, limit: request.evidence_limit },
    );
  }

  async cancelInvocation(value: unknown): Promise<ExecuteInvocationV2Result> {
    const request = validateCancelInvocationV2Request(value);
    const owner = await this.authorizer({ operation: "cancel", request });
    let record = await this.requireRecord(request.invocation_id, owner);
    if (record.status !== "running") {
      return this.resultForRecord(undefined, record, {
        after: 0,
        limit: MAX_EVIDENCE_PAGE_SIZE,
      });
    }
    await this.runtime.cancel(request, record.session_id);
    const result = await this.finishTerminal(
      record,
      {
        status: "cancelled",
        error: { code: "cancelled", message: request.reason },
      },
      [],
      attributionFromRecord(record),
      "reused",
    );
    if (result !== null) return result;
    record = await this.requireRecord(request.invocation_id, owner);
    return this.resultForRecord(undefined, record, {
      after: 0,
      limit: MAX_EVIDENCE_PAGE_SIZE,
    });
  }

  private async resultForRecord(
    request: ExecuteInvocationV2Request | undefined,
    record: ExecutionRecord,
    cursor: { readonly after: number; readonly limit: number },
  ): Promise<ExecuteInvocationV2Result> {
    if (record.status === "running") {
      if (record.lease_expires_at_ms > this.now()) {
        return {
          ...baseResult(
            record,
            request === undefined
              ? attributionFromRecord(record)
              : attributionFromRequest(request, null),
            emptyEvidence(cursor.after),
            "reused",
          ),
          session_ref: this.positionalSessionRef(record.session_id, 0),
          status: "running",
          retry_after_ms: Math.min(
            5_000,
            Math.max(1, record.lease_expires_at_ms - this.now()),
          ),
        };
      }
      const interrupted = await this.finishTerminal(
        record,
        {
          status: "interrupted",
          error: {
            code: "runtime_interrupted",
            message: "Pillbox managed invocation owner lease expired",
          },
        },
        [],
        request === undefined
          ? attributionFromRecord(record)
          : attributionFromRequest(request, null),
        "reused",
      );
      if (interrupted !== null) return interrupted;
      return this.resultForRecord(
        request,
        await this.requireRecord(record.invocation_id, record.owner),
        cursor,
      );
    }
    if (record.artifact_ref === undefined) {
      throw new Error(`terminal execution '${record.invocation_id}' has no artifact`);
    }
    const artifact = await this.artifacts.read(record.artifact_ref);
    const stored = this.terminalWithSessionRef(record, artifact);
    return {
      ...stored,
      disposition: "reused",
      evidence: evidencePage(artifact, record, cursor.after, cursor.limit),
      ...(artifact.cost === undefined
        ? {}
        : { cost: artifact.cost as unknown as RunCostEnvelope }),
    };
  }

  private async finishTurn(
    request: ExecuteInvocationV2Request,
    record: ExecutionRecord,
    turn: RuntimeTurnResult,
    disposition: "created" | "reused",
  ): Promise<ExecuteInvocationV2Result> {
    const attribution = attributionFromRequest(request, turn.served_model);
    const terminal = turn.error
      ? ({ status: errorStatus(turn.error.code), error: turn.error } as const)
      : ({ status: "completed", output: turn.output ?? {} } as const);
    const result = await this.finishTerminal(
      record,
      terminal,
      turn.evidence,
      attribution,
      disposition,
    );
    if (result === null) {
      return this.resultForRecord(request, await this.requireRecord(record.invocation_id, record.owner), {
        after: 0,
        limit: MAX_EVIDENCE_PAGE_SIZE,
      });
    }
    return result;
  }

  private async finishTerminal(
    record: ExecutionRecord,
    terminal: ExecutionTerminal,
    evidence: readonly JsonValue[],
    attribution: ExecutionAttribution,
    disposition: "created" | "reused",
  ): Promise<ExecuteInvocationV2Result | null> {
    const outcome = terminalOutcome(terminal, evidence.length);
    const base = baseResult(record, attribution, emptyEvidence(0), disposition);
    const placeholder: ExecuteInvocationV2Result =
      outcome.status === "completed"
        ? {
            ...base,
            session_ref: this.completedSessionRef(record.session_id, evidence.length),
            ...outcome,
          }
        : {
            ...base,
            session_ref: this.positionalSessionRef(record.session_id, evidence.length),
            ...outcome,
          };
    this.costMeter?.observeEvidence(evidence);
    const cost = this.costMeter?.terminal(outcome.status, {
      sandbox_duration_ms: Math.max(0, this.now() - record.created_at_ms),
      sandbox_profile: this.sandboxProfile,
      planned_d1_terminal_writes: 1,
      planned_r2_writes: 1,
      planned_analytics_points: this.analytics === undefined ? 0 : 1,
    });
    const unsealedArtifact: ExecutionArtifact = {
      version: 1,
      invocation_id: record.invocation_id,
      request_hash: record.request_hash,
      terminal_result: placeholder as unknown as JsonValue,
      evidence,
      ...(cost === undefined ? {} : { cost: cost as unknown as JsonValue }),
    };
    let artifact = sealArtifactCostBytes(unsealedArtifact);
    let artifactRef;
    try {
      artifactRef = await this.artifacts.write(artifact);
    } catch (cause) {
      if (!(cause instanceof ExecutionArtifactConflictError)) throw cause;
      artifact = cause.existing;
      artifactRef = cause.existing_ref;
    }
    const stored = this.terminalWithSessionRef(record, artifact);
    const finished = await this.store.finish({
      invocation_id: record.invocation_id,
      request_hash: record.request_hash,
      owner_token: record.owner_token,
      owner: record.owner,
      status: stored.status,
      artifact_ref: artifactRef,
      now_ms: this.now(),
    });
    if (!finished) return null;
    // The terminal CAS is the emission fence: its sole winner may attempt once,
    // and a crash or failure after this point is reconciled as observed variance.
    await this.emitTerminalAnalytics(record, stored, artifact);
    const result: ExecuteInvocationV2Result = {
      ...stored,
      disposition,
      evidence: evidencePage(
        artifact,
        { ...record, artifact_ref: artifactRef },
        0,
        MAX_EVIDENCE_PAGE_SIZE,
      ),
      ...(artifact.cost === undefined
        ? {}
        : { cost: artifact.cost as unknown as RunCostEnvelope }),
    };
    return result;
  }

  private async emitTerminalAnalytics(
    record: ExecutionRecord,
    terminal: Extract<
      ExecuteInvocationV2Result,
      { readonly status: "completed" | "failed" | "cancelled" | "interrupted" }
    >,
    artifact: ExecutionArtifact,
  ): Promise<void> {
    if (this.analytics === undefined || artifact.cost === undefined) return;
    const cost = artifact.cost as unknown as RunCostEnvelope;
    if (cost.infrastructure.analytics_points_planned !== 1) return;
    try {
      await this.analytics.emit({
        invocation_id: record.invocation_id,
        request_hash: record.request_hash,
        harness: terminal.attribution.harness,
        transport: terminal.attribution.transport,
        cost,
      });
    } catch (cause) {
      console.warn(
        "Pillbox terminal Analytics emission failed after authoritative commit:",
        safeHuddlesRuntimeDiagnostic(cause),
      );
    }
  }

  private conflictResult(
    request: ExecuteInvocationV2Request,
    record: ExecutionRecord,
    requestedHash: `sha256:${string}`,
  ): ExecuteInvocationV2Result {
    return {
      ...baseResult(
        record,
        attributionFromRequest(request, null),
        emptyEvidence(0),
        "reused",
      ),
      session_ref: this.positionalSessionRef(record.session_id, 0),
      status: "conflict",
      error: {
        code: "idempotency_conflict",
        message: "invocation or idempotency key is already bound to different content",
        existing_request_hash: record.request_hash,
        requested_request_hash: requestedHash,
      },
    };
  }

  private reservationConflictResult(
    request: ExecuteInvocationV2Request,
    existingHash: `sha256:${string}`,
    requestedHash: `sha256:${string}`,
    executionDigest: `sha256:${string}`,
  ): ExecuteInvocationV2Result {
    return {
      disposition: "reused",
      invocation_id: request.invocation_id,
      request_hash: existingHash,
      execution_digest: executionDigest,
      execution_policy_revision: request.execution_policy_revision,
      session_ref: this.positionalSessionRef(request.session_ref.session_id, 0),
      attribution: attributionFromRequest(request, null),
      evidence: emptyEvidence(0),
      status: "conflict",
      error: {
        code: "idempotency_conflict",
        message: "invocation is already reserved for different content",
        existing_request_hash: existingHash,
        requested_request_hash: requestedHash,
      },
    };
  }

  private managedDisabledResult(
    request: ExecuteInvocationV2Request,
    requestHash: `sha256:${string}`,
    executionDigest: `sha256:${string}`,
    message: string,
  ): ExecuteInvocationV2Result {
    return this.preClaimFailureResult(request, requestHash, executionDigest, {
      code: "managed_disabled",
      message,
    });
  }

  private preClaimFailureResult(
    request: ExecuteInvocationV2Request,
    requestHash: `sha256:${string}`,
    executionDigest: `sha256:${string}`,
    error: {
      readonly code: "managed_disabled" | "unsupported_execution" | "unsupported_policy";
      readonly message: string;
    },
  ): ExecuteInvocationV2Result {
    return {
      disposition: "created",
      invocation_id: request.invocation_id,
      request_hash: requestHash,
      execution_digest: executionDigest,
      execution_policy_revision: request.execution_policy_revision,
      session_ref: this.positionalSessionRef(request.session_ref.session_id, 0),
      attribution: attributionFromRequest(request, null),
      evidence: emptyEvidence(0),
      status: "failed",
      error,
    };
  }

  private async requireRecord(
    invocation_id: string,
    owner: ManagedExecutionOwner,
  ): Promise<ExecutionRecord> {
    const record = await this.store.get(invocation_id, owner);
    if (record === null) throw new ExecutionNotFoundError(invocation_id);
    return record;
  }

  private positionalSessionRef(
    session_id: string,
    evidenceLength: number,
  ): ExecutionResultSessionRef {
    return evidenceLength === 0
      ? { session_id }
      : { session_id, seq_range: [0, evidenceLength - 1] };
  }

  private completedSessionRef(
    session_id: string,
    evidenceLength: number,
  ): CompletedExecutionResultSessionRef {
    if (evidenceLength === 0) {
      throw new Error("completed execution has no immutable positional evidence");
    }
    return { session_id, seq_range: [0, evidenceLength - 1] };
  }

  private terminalWithSessionRef(
    record: ExecutionRecord,
    artifact: ExecutionArtifact,
  ): Extract<
    ExecuteInvocationV2Result,
    { readonly status: "completed" | "failed" | "cancelled" | "interrupted" }
  > {
    const terminal = terminalResult(artifact, record);
    if (terminal.status === "completed" && artifact.evidence.length === 0) {
      const { output: _output, ...base } = terminal;
      return {
        ...base,
        status: "failed",
        session_ref: this.positionalSessionRef(record.session_id, 0),
        error: MISSING_POSITIONAL_EVIDENCE_ERROR,
      };
    }
    return terminal.status === "completed"
      ? {
          ...terminal,
          session_ref: this.completedSessionRef(
            record.session_id,
            artifact.evidence.length,
          ),
        }
      : {
          ...terminal,
          session_ref: this.positionalSessionRef(
            record.session_id,
            artifact.evidence.length,
          ),
        };
  }
}

function unsupportedManagedRequest(
  request: ExecuteInvocationV2Request,
):
  | {
      readonly code: "unsupported_execution" | "unsupported_policy";
      readonly message: string;
    }
  | undefined {
  if (request.tool_policy !== "deny_all") {
    return {
      code: "unsupported_policy",
      message:
        "managed execution requires tool_policy 'deny_all' until credentials are brokered",
    };
  }
  const { harness, transport } = request.execution.transport;
  if (
    harness !== "opencode" ||
    (transport !== "http" && transport !== "cloudflare-service-binding")
  ) {
    return {
      code: "unsupported_execution",
      message: `unsupported managed execution ${harness}/${transport}`,
    };
  }
  return undefined;
}

type SandboxHandle = ReturnType<typeof getSandbox>;

export interface OpencodeExecutionRuntimeOptions {
  readonly sandboxFor: (session_id: string) => Promise<SandboxHandle> | SandboxHandle;
  readonly configFor: (
    request: ExecuteInvocationV2Request,
  ) => Promise<{ readonly config?: unknown; readonly env: Readonly<Record<string, string>> }> | {
    readonly config?: unknown;
    readonly env: Readonly<Record<string, string>>;
  };
}

export class OpencodeExecutionRuntime implements ExecutionRuntime {
  private readonly options: OpencodeExecutionRuntimeOptions;

  constructor(options: OpencodeExecutionRuntimeOptions) {
    this.options = options;
  }

  async execute(request: ExecuteInvocationV2Request): Promise<RuntimeTurnResult> {
    const evidence: JsonValue[] = [];
    let text = "";
    let evidenceBytes = 0;
    let outputBytes = 0;
    const contentBudget = MAX_EXECUTION_ARTIFACT_BYTES - 64 * 1024;
    const appendEvidence = (value: JsonValue): void => {
      const bytes = new TextEncoder().encode(JSON.stringify(value)).byteLength;
      if (bytes > MAX_EXECUTION_EVIDENCE_EVENT_BYTES) {
        throw new Error(`managed evidence event exceeds ${MAX_EXECUTION_EVIDENCE_EVENT_BYTES} bytes`);
      }
      evidenceBytes += bytes;
      if (evidenceBytes + outputBytes > contentBudget) {
        throw new Error(`managed execution content exceeds ${contentBudget} bytes`);
      }
      evidence.push(value);
    };
    const appendOutput = (value: string): void => {
      outputBytes += new TextEncoder().encode(value).byteLength;
      if (evidenceBytes + outputBytes > contentBudget) {
        throw new Error(`managed execution content exceeds ${contentBudget} bytes`);
      }
      text += value;
    };
    let runtimeError: { code: ExecuteInvocationV2ErrorCode; message: string } | undefined;
    const structured = await driveOpencodeTurn({
      sandbox: await this.options.sandboxFor(request.session_ref.session_id),
      text: request.rendered_input,
      model: `${request.execution.requested.provider}/${request.execution.requested.model}`,
      toolPolicy: request.tool_policy === "deny_all" ? "deny_all" : undefined,
      outputFormat:
        request.output_format.type === "json_schema"
          ? request.output_format
          : undefined,
      config: await this.options.configFor(request),
      sink: {
        appendAgent(payload: Payload) {
          appendEvidence(payload as unknown as JsonValue);
          if (payload.type === "message_delta" && typeof payload.text === "string") {
            appendOutput(payload.text);
          }
          if (
            payload.type === "attention_required" &&
            payload.reason === "error_stalled"
          ) {
            runtimeError = {
              code: "runtime_failed",
              message:
                typeof payload.message === "string" && payload.message.length > 0
                  ? payload.message
                  : "agent turn failed",
            };
          }
        },
        appendError(message: string) {
          appendEvidence({
            type: "attention_required",
            reason: "error_stalled",
            message,
          });
          runtimeError = { code: "runtime_failed", message };
        },
        appendSystemTool(item) {
          appendEvidence({
            type: "tool_call",
            toolCallId: `${item.idPrefix}:${evidence.length + 1}`,
            name: item.name,
            status: "completed",
            ...(item.input === undefined ? {} : { input: item.input }),
            output: item.output,
          } as unknown as JsonValue);
        },
      },
    });
    if (runtimeError !== undefined) {
      return { served_model: null, error: runtimeError, evidence };
    }
    const output = structured ?? text;
    if (structured !== undefined) {
      outputBytes += new TextEncoder().encode(structured).byteLength;
      if (evidenceBytes + outputBytes > contentBudget) {
        throw new Error(`managed execution content exceeds ${contentBudget} bytes`);
      }
    }
    if (output.trim().length === 0) {
      return {
        served_model: null,
        error: {
          code:
            request.output_format.type === "json_schema"
              ? "structured_output_missing"
              : "runtime_failed",
          message:
            request.output_format.type === "json_schema"
              ? "agent turn produced no structured output"
              : "agent turn produced no text output",
        },
        evidence,
      };
    }
    return {
      served_model: `${request.execution.requested.provider}/${request.execution.requested.model}`,
      output:
        structured === undefined
          ? { text: output }
          : { json: JSON.parse(structured) as JsonValue },
      evidence,
    };
  }

  async cancel(
    _request: CancelInvocationV2Request,
    session_id: string,
  ): Promise<void> {
    const sandbox = await this.options.sandboxFor(session_id);
    await sandbox.killAllProcesses();
  }
}

function baseResult(
  record: ExecutionRecord,
  attribution: ExecutionAttribution,
  evidence: ExecutionEvidencePage,
  disposition: "created" | "reused",
) {
  return {
    disposition,
    invocation_id: record.invocation_id,
    request_hash: record.request_hash,
    execution_digest: record.execution_digest,
    execution_policy_revision: record.execution_policy_revision,
    session_ref: { session_id: record.session_id },
    attribution,
    evidence,
  } as const;
}

function attributionFromRequest(
  request: ExecuteInvocationV2Request,
  served_model: string | null,
): ExecutionAttribution {
  return {
    harness: request.execution.transport.harness,
    transport: request.execution.transport.transport,
    requested_model: `${request.execution.requested.provider}/${request.execution.requested.model}`,
    served_model,
  };
}

function attributionFromRecord(record: ExecutionRecord): ExecutionAttribution {
  return record.attribution;
}

function errorStatus(
  code: ExecuteInvocationV2ErrorCode,
): "failed" | "cancelled" | "interrupted" {
  if (code === "cancelled") return "cancelled";
  if (code === "runtime_interrupted") return "interrupted";
  return "failed";
}

function terminalOutcome(
  terminal: ExecutionTerminal,
  evidenceLength: number,
): ExecutionTerminal {
  return terminal.status === "completed" && evidenceLength === 0
    ? { status: "failed", error: MISSING_POSITIONAL_EVIDENCE_ERROR }
    : terminal;
}

function emptyEvidence(from: number): ExecutionEvidencePage {
  return { from, next: null, truncated: false, events: [] };
}

function evidencePage(
  artifact: ExecutionArtifact,
  record: ExecutionRecord,
  after: number,
  limit: number,
): ExecutionEvidencePage {
  const events = artifact.evidence.slice(after, after + limit);
  const next = after + events.length < artifact.evidence.length
    ? after + events.length
    : null;
  return {
    from: after,
    next,
    truncated: next !== null,
    events,
    ...(record.artifact_ref === undefined
      ? {}
      : { artifact_ref: record.artifact_ref }),
  };
}

function terminalResult(
  artifact: ExecutionArtifact,
  record: ExecutionRecord,
): Extract<
  ExecuteInvocationV2Result,
  { readonly status: "completed" | "failed" | "cancelled" | "interrupted" }
> {
  const result = artifact.terminal_result as unknown as ExecuteInvocationV2Result;
  if (
    result.invocation_id !== record.invocation_id ||
    result.request_hash !== record.request_hash ||
    result.session_ref.session_id !== record.session_id ||
    result.status !== "completed" &&
    result.status !== "failed" &&
    result.status !== "cancelled" &&
    result.status !== "interrupted"
  ) {
    throw new Error("execution artifact does not contain a terminal result");
  }
  return result;
}
