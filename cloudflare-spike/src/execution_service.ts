import type { getSandbox } from "@cloudflare/sandbox";
import {
  computeExecutionIdentityDigest,
  computeInvocationRequestHash,
  type CancelInvocationV2Request,
  type ExecuteInvocationV2ErrorCode,
  type ExecuteInvocationV2Request,
  type ExecuteInvocationV2Result,
  type ExecutionAttribution,
  type ExecutionEvidencePage,
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
import type {
  ExecutionClaimInput,
  ExecutionRecord,
  ExecutionStore,
} from "./execution_store.js";
import { safeHuddlesRuntimeDiagnostic } from "./huddles_policy.js";
import { driveOpencodeTurn } from "./opencode_turn.js";
import type { Payload } from "./contract.js";
import {
  type RunCostAnalytics,
  type RunCostEnvelope,
  type RunCostMeter,
  sealArtifactCostBytes,
} from "./run_cost.js";

export const EXECUTION_OWNER_LEASE_MS = 10 * 60 * 1_000;

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
  cancel(request: CancelInvocationV2Request): Promise<void>;
}

export interface ExecutionServiceOptions {
  readonly now?: () => number;
  readonly ownerToken?: () => string;
  readonly costMeter?: RunCostMeter;
  readonly analytics?: RunCostAnalytics;
  readonly sandboxProfile?: string;
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

  constructor(
    store: ExecutionStore,
    artifacts: ExecutionArtifactStore,
    runtime: ExecutionRuntime,
    options: ExecutionServiceOptions = {},
  ) {
    this.store = store;
    this.artifacts = artifacts;
    this.runtime = runtime;
    this.now = options.now ?? Date.now;
    this.ownerToken = options.ownerToken ?? (() => crypto.randomUUID());
    this.costMeter = options.costMeter;
    this.analytics = options.analytics;
    this.sandboxProfile = options.sandboxProfile ?? null;
  }

  async executeInvocation(value: unknown): Promise<ExecuteInvocationV2Result> {
    const request = await validateExecuteInvocationV2Request(value);
    const requestHash = await computeInvocationRequestHash(request);
    const executionDigest = await computeExecutionIdentityDigest(
      request.execution,
      request.execution_policy_revision,
    );
    const now = this.now();
    const input: ExecutionClaimInput = {
      invocation_id: request.invocation_id,
      idempotency_key: request.idempotency_key,
      request_hash: requestHash,
      execution_digest: executionDigest,
      execution_policy_revision: request.execution_policy_revision,
      session_id: request.session_ref.session_id,
      attribution: attributionFromRequest(request, null),
      owner_token: this.ownerToken(),
      now_ms: now,
      lease_expires_at_ms: now + EXECUTION_OWNER_LEASE_MS,
    };
    const claim = await this.store.claim(input);
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
    if (
      request.execution.transport.harness !== "opencode" ||
      request.execution.transport.transport !== "http"
    ) {
      turn = {
        served_model: null,
        error: {
          code: "unsupported_execution",
          message: `unsupported managed execution ${request.execution.transport.harness}/${request.execution.transport.transport}`,
        },
        evidence: [],
      };
    } else {
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
    }
    return this.finishTurn(request, claim.record, turn, "created");
  }

  async getExecutionStatus(value: unknown): Promise<ExecuteInvocationV2Result> {
    const request = validateGetInvocationV2Request(value);
    const record = await this.requireRecord(request.invocation_id);
    return this.resultForRecord(
      undefined,
      record,
      { after: request.evidence_after, limit: request.evidence_limit },
    );
  }

  async cancelInvocation(value: unknown): Promise<ExecuteInvocationV2Result> {
    const request = validateCancelInvocationV2Request(value);
    let record = await this.requireRecord(request.invocation_id);
    if (record.status !== "running") {
      return this.resultForRecord(undefined, record, {
        after: 0,
        limit: MAX_EVIDENCE_PAGE_SIZE,
      });
    }
    await this.runtime.cancel(request);
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
    record = await this.requireRecord(request.invocation_id);
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
        await this.requireRecord(record.invocation_id),
        cursor,
      );
    }
    if (record.artifact_ref === undefined) {
      throw new Error(`terminal execution '${record.invocation_id}' has no artifact`);
    }
    const artifact = await this.artifacts.read(record.artifact_ref);
    const stored = artifact.terminal_result as unknown as ExecuteInvocationV2Result;
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
      return this.resultForRecord(request, await this.requireRecord(record.invocation_id), {
        after: 0,
        limit: MAX_EVIDENCE_PAGE_SIZE,
      });
    }
    return result;
  }

  private async finishTerminal(
    record: ExecutionRecord,
    terminal:
      | { readonly status: "completed"; readonly output: { readonly text?: string; readonly json?: JsonValue } }
      | {
          readonly status: "failed" | "cancelled" | "interrupted";
          readonly error: { readonly code: ExecuteInvocationV2ErrorCode; readonly message: string };
        },
    evidence: readonly JsonValue[],
    attribution: ExecutionAttribution,
    disposition: "created" | "reused",
  ): Promise<ExecuteInvocationV2Result | null> {
    const placeholder = {
      ...baseResult(record, attribution, emptyEvidence(0), disposition),
      ...terminal,
    } satisfies ExecuteInvocationV2Result;
    this.costMeter?.observeEvidence(evidence);
    const cost = this.costMeter?.terminal(terminal.status, {
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
    const artifact = sealArtifactCostBytes(unsealedArtifact);
    const artifactRef = await this.artifacts.write(artifact);
    const finished = await this.store.finish({
      invocation_id: record.invocation_id,
      request_hash: record.request_hash,
      owner_token: record.owner_token,
      status: terminal.status,
      artifact_ref: artifactRef,
      now_ms: this.now(),
    });
    if (!finished) return null;
    const result: ExecuteInvocationV2Result = {
      ...placeholder,
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
    if (this.analytics !== undefined && result.cost !== undefined) {
      try {
        await this.analytics.emit({
          invocation_id: record.invocation_id,
          request_hash: record.request_hash,
          harness: attribution.harness,
          transport: attribution.transport,
          cost: result.cost,
        });
      } catch (cause) {
        console.error(
          "run cost analytics emission failed",
          safeHuddlesRuntimeDiagnostic(cause),
        );
      }
    }
    return result;
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
      request_hash: requestedHash,
      status: "conflict",
      error: {
        code: "idempotency_conflict",
        message: "invocation or idempotency key is already bound to different content",
      },
    };
  }

  private async requireRecord(invocation_id: string): Promise<ExecutionRecord> {
    const record = await this.store.get(invocation_id);
    if (record === null) throw new ExecutionNotFoundError(invocation_id);
    return record;
  }
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
    let runtimeError: { code: ExecuteInvocationV2ErrorCode; message: string } | undefined;
    const structured = await driveOpencodeTurn({
      sandbox: await this.options.sandboxFor(request.session_ref.session_id),
      text: request.rendered_input,
      model: `${request.execution.requested.provider}/${request.execution.requested.model}`,
      toolPolicy: request.tool_policy,
      outputFormat: request.output_format,
      config: await this.options.configFor(request),
      sink: {
        appendAgent(payload: Payload) {
          evidence.push(payload as unknown as JsonValue);
          if (payload.type === "message_delta" && typeof payload.text === "string") {
            text += payload.text;
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
          evidence.push({
            type: "attention_required",
            reason: "error_stalled",
            message,
          });
          runtimeError = { code: "runtime_failed", message };
        },
        appendSystemTool(item) {
          evidence.push({
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
    if (output.trim().length === 0) {
      return {
        served_model: null,
        error: {
          code: "structured_output_missing",
          message: "agent turn produced no structured output",
        },
        evidence,
      };
    }
    return {
      served_model: null,
      output: structured === undefined ? { text: output } : { text: structured },
      evidence,
    };
  }

  async cancel(request: CancelInvocationV2Request): Promise<void> {
    const sandbox = await this.options.sandboxFor(request.invocation_id);
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
