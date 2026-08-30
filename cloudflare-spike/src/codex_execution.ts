import type { RunCostEnvelope } from "./run_cost.js";

export type JsonPrimitive = null | boolean | number | string;
export type JsonValue =
  | JsonPrimitive
  | JsonValue[]
  | { readonly [key: string]: JsonValue };

export type Harness = "claude_code" | "codex" | "opencode" | "pi" | "custom";
export type ReasoningEffort = "low" | "medium" | "high";

/** The broad InvocationExecution wire shape owned by Huddles. */
export interface HarnessTransport {
  readonly harness: Harness;
  readonly transport: string;
  readonly harness_version: string;
  readonly adapter_revision: string;
}

export interface RequestedModelProfile {
  readonly provider: string;
  readonly model: string;
  readonly profile: string | null;
  readonly reasoning_effort: ReasoningEffort;
}

export interface InvocationExecution {
  readonly transport: HarnessTransport;
  readonly requested: RequestedModelProfile;
  readonly placement?: "local_microvm" | "managed_container";
  readonly context_renderer_revision: string;
  readonly verifier_ref?: string;
}

export type SupportedCodexExecution = InvocationExecution & {
  readonly transport: HarnessTransport & {
    readonly harness: "codex";
    readonly transport: "app_server";
  };
};

/** The generic ACP capability, independent of the selected harness. */
export type SupportedAcpExecution = InvocationExecution & {
  readonly transport: HarnessTransport & {
    readonly transport: "acp";
  };
};

export interface JsonSchemaOutputFormat {
  readonly type: "json_schema";
  readonly schema: { readonly [key: string]: JsonValue };
  readonly retry_count: 2;
}

export type Sha256Digest = `sha256:${string}`;
export type ExecutionDigest = Sha256Digest;
export type InvocationRequestHash = Sha256Digest;
export const MAX_EVIDENCE_PAGE_SIZE = 100;

export interface ExecuteInvocationV2Request {
  readonly contract_version: "pillbox.execution/2";
  readonly session_ref: { readonly session_id: string };
  readonly invocation_id: string;
  readonly idempotency_key: string;
  readonly rendered_input: string;
  readonly rendered_input_hash: Sha256Digest;
  readonly tool_policy: "deny_all";
  readonly execution: InvocationExecution;
  readonly execution_policy_revision: string;
  readonly output_format: JsonSchemaOutputFormat;
}

export type ExecuteInvocationV2ErrorCode =
  | "idempotency_conflict"
  | "unsupported_execution"
  | "unsupported_policy"
  | "auth_unavailable"
  | "runtime_busy"
  | "runtime_interrupted"
  | "runtime_failed"
  | "cancelled"
  | "structured_output_missing";

export interface ExecutionAttribution {
  readonly harness: Harness;
  readonly transport: string;
  readonly requested_model: string;
  readonly served_model: string | null;
}

export interface ExecutionArtifactRef {
  readonly key: string;
  readonly media_type: "application/json";
  readonly bytes: number;
  readonly sha256: Sha256Digest;
}

/** A bounded projection of runtime evidence; the immutable artifact is canonical. */
export interface ExecutionEvidencePage {
  readonly from: number;
  readonly next: number | null;
  readonly truncated: boolean;
  readonly events: readonly JsonValue[];
  readonly artifact_ref?: ExecutionArtifactRef;
}

export interface GetInvocationV2Request {
  readonly contract_version: "pillbox.execution/2";
  readonly invocation_id: string;
  readonly evidence_after: number;
  readonly evidence_limit: number;
}

export interface CancelInvocationV2Request {
  readonly contract_version: "pillbox.execution/2";
  readonly invocation_id: string;
  readonly idempotency_key: string;
  readonly reason: string;
}

interface ExecuteInvocationV2ResultBase {
  readonly disposition: "created" | "reused";
  readonly invocation_id: string;
  readonly request_hash: InvocationRequestHash;
  readonly execution_digest: ExecutionDigest;
  readonly execution_policy_revision: string;
  readonly session_ref: {
    readonly session_id: string;
    readonly seq_range?: readonly [number, number];
  };
  readonly attribution: ExecutionAttribution;
  readonly evidence: ExecutionEvidencePage;
  readonly cost?: RunCostEnvelope;
}

export type ExecuteInvocationV2Result =
  | (ExecuteInvocationV2ResultBase & {
      readonly status: "running";
      readonly retry_after_ms: number;
    })
  | (ExecuteInvocationV2ResultBase & {
      readonly status: "completed";
      readonly output: {
        readonly text?: string;
        readonly json?: JsonValue;
      };
    })
  | (ExecuteInvocationV2ResultBase & {
      readonly status: "failed" | "cancelled" | "interrupted" | "conflict";
      readonly error: {
        readonly code: ExecuteInvocationV2ErrorCode;
        readonly message: string;
      };
    });

export class ExecutionBoundaryError extends Error {
  readonly code = "invalid_execute_invocation_v2_request" as const;

  constructor(message: string) {
    super(message);
    this.name = "ExecutionBoundaryError";
  }
}

/** Compatibility name retained while callers move to the runtime-owned boundary. */
export { ExecutionBoundaryError as CodexExecutionBoundaryError };

export class UnsupportedCodexExecutionError extends Error {
  readonly code = "unsupported_execution" as const;

  constructor(message: string) {
    super(message);
    this.name = "UnsupportedCodexExecutionError";
  }
}

export class UnsupportedAcpExecutionError extends Error {
  readonly code = "unsupported_execution" as const;

  constructor(message: string) {
    super(message);
    this.name = "UnsupportedAcpExecutionError";
  }
}

/** Hash the exact UTF-8 rendered input used by the invocation. */
export async function computeRenderedInputHash(
  rendered_input: string,
): Promise<Sha256Digest> {
  return sha256Digest(rendered_input);
}

/** Hash the complete sealed execution identity, including its policy revision. */
export async function computeExecutionIdentityDigest(
  execution: InvocationExecution,
  execution_policy_revision: string,
): Promise<ExecutionDigest> {
  return sha256Json({
    execution: execution as unknown as JsonValue,
    execution_policy_revision,
  } as JsonValue);
}

/** Compatibility alias for callers that used the earlier contract helper name. */
export const canonicalExecutionDigest = computeExecutionIdentityDigest;

/** Hash a validated request for durable idempotency and conflict detection. */
export async function computeInvocationRequestHash(
  request: ExecuteInvocationV2Request,
): Promise<InvocationRequestHash> {
  return sha256Json(request as unknown as JsonValue);
}

/** Validate the private boundary before crossing into a Worker RPC. */
export async function validateExecuteInvocationV2Request(
  value: unknown,
): Promise<ExecuteInvocationV2Request> {
  assertJsonValue(value, "request");
  const request = requireObject(value, "request");
  assertExactKeys(
    request,
    [
      "contract_version",
      "session_ref",
      "invocation_id",
      "idempotency_key",
      "rendered_input",
      "rendered_input_hash",
      "tool_policy",
      "execution",
      "execution_policy_revision",
      "output_format",
    ],
    "request",
  );

  if (request.contract_version !== "pillbox.execution/2") {
    reject("contract_version must be 'pillbox.execution/2'");
  }

  const sessionRef = requireObject(request.session_ref, "session_ref");
  assertExactKeys(sessionRef, ["session_id"], "session_ref");
  const sessionId = requireNonEmptyString(
    sessionRef.session_id,
    "session_ref.session_id",
  );
  const invocationId = requireNonEmptyString(request.invocation_id, "invocation_id");
  const idempotencyKey = requireNonEmptyString(
    request.idempotency_key,
    "idempotency_key",
  );
  const renderedInput = requireNonEmptyString(
    request.rendered_input,
    "rendered_input",
  );
  const renderedInputHash = requireDigest(
    request.rendered_input_hash,
    "rendered_input_hash",
  );
  const expectedInputHash = await computeRenderedInputHash(renderedInput);
  if (renderedInputHash !== expectedInputHash) {
    reject("rendered_input_hash does not match rendered_input");
  }
  if (request.tool_policy !== "deny_all") {
    reject("tool_policy must be 'deny_all'");
  }

  const executionPolicyRevision = requireNonEmptyString(
    request.execution_policy_revision,
    "execution_policy_revision",
  );
  const execution = validateInvocationExecution(request.execution);
  const outputFormat = validateOutputFormat(request.output_format);

  return {
    contract_version: "pillbox.execution/2",
    session_ref: { session_id: sessionId },
    invocation_id: invocationId,
    idempotency_key: idempotencyKey,
    rendered_input: renderedInput,
    rendered_input_hash: renderedInputHash,
    tool_policy: "deny_all",
    execution,
    execution_policy_revision: executionPolicyRevision,
    output_format: outputFormat,
  };
}

/** Validate a bounded status/evidence read. Missing cursor fields use safe defaults. */
export function validateGetInvocationV2Request(
  value: unknown,
): GetInvocationV2Request {
  assertJsonValue(value, "request");
  const request = requireObject(value, "request");
  assertExactKeys(
    request,
    ["contract_version", "invocation_id", "evidence_after", "evidence_limit"],
    "request",
  );
  requireContractVersion(request.contract_version);
  const invocationId = requireNonEmptyString(request.invocation_id, "invocation_id");
  const evidenceAfter =
    request.evidence_after === undefined
      ? 0
      : requireNonNegativeInteger(request.evidence_after, "evidence_after");
  const evidenceLimit =
    request.evidence_limit === undefined
      ? MAX_EVIDENCE_PAGE_SIZE
      : requirePositiveInteger(request.evidence_limit, "evidence_limit");
  if (evidenceLimit > MAX_EVIDENCE_PAGE_SIZE) {
    reject(`evidence_limit must be <= ${MAX_EVIDENCE_PAGE_SIZE}`);
  }
  return {
    contract_version: "pillbox.execution/2",
    invocation_id: invocationId,
    evidence_after: evidenceAfter,
    evidence_limit: evidenceLimit,
  };
}

/** Validate an idempotent cancellation intent without importing scheduler state. */
export function validateCancelInvocationV2Request(
  value: unknown,
): CancelInvocationV2Request {
  assertJsonValue(value, "request");
  const request = requireObject(value, "request");
  assertExactKeys(
    request,
    ["contract_version", "invocation_id", "idempotency_key", "reason"],
    "request",
  );
  requireContractVersion(request.contract_version);
  return {
    contract_version: "pillbox.execution/2",
    invocation_id: requireNonEmptyString(request.invocation_id, "invocation_id"),
    idempotency_key: requireNonEmptyString(
      request.idempotency_key,
      "idempotency_key",
    ),
    reason: requireNonEmptyString(request.reason, "reason"),
  };
}

/** Refine a valid broad Huddles execution to the currently implemented Codex capability. */
export function validateSupportedCodexExecution(
  execution: InvocationExecution,
): SupportedCodexExecution {
  if (execution.transport.harness !== "codex") {
    throw new UnsupportedCodexExecutionError(
      `harness '${execution.transport.harness}' is not supported by the Codex adapter`,
    );
  }
  if (execution.transport.transport !== "app_server") {
    throw new UnsupportedCodexExecutionError(
      `Codex transport '${execution.transport.transport}' is not supported`,
    );
  }
  return execution as SupportedCodexExecution;
}

/** Refine a valid broad Huddles execution to the generic ACP capability. */
export function validateSupportedAcpExecution(
  execution: InvocationExecution,
): SupportedAcpExecution {
  if (execution.transport.transport !== "acp") {
    throw new UnsupportedAcpExecutionError(
      `transport '${execution.transport.transport}' is not supported by the ACP adapter`,
    );
  }
  return execution as SupportedAcpExecution;
}

function validateInvocationExecution(value: JsonValue | undefined): InvocationExecution {
  const execution = requireObject(value, "execution");
  assertExactKeys(
    execution,
    ["transport", "requested", "placement", "context_renderer_revision", "verifier_ref"],
    "execution",
  );

  const transport = requireObject(execution.transport, "execution.transport");
  assertExactKeys(
    transport,
    ["harness", "transport", "harness_version", "adapter_revision"],
    "execution.transport",
  );
  if (
    transport.harness !== "claude_code" &&
    transport.harness !== "codex" &&
    transport.harness !== "opencode" &&
    transport.harness !== "pi" &&
    transport.harness !== "custom"
  ) {
    reject("execution.transport.harness is invalid");
  }
  const transportName = requireNonEmptyString(
    transport.transport,
    "execution.transport.transport",
  );
  const harnessVersion = requireNonEmptyString(
    transport.harness_version,
    "execution.transport.harness_version",
  );
  const adapterRevision = requireNonEmptyString(
    transport.adapter_revision,
    "execution.transport.adapter_revision",
  );

  const requested = requireObject(execution.requested, "execution.requested");
  assertExactKeys(
    requested,
    ["provider", "model", "profile", "reasoning_effort"],
    "execution.requested",
  );
  const provider = requireNonEmptyString(
    requested.provider,
    "execution.requested.provider",
  );
  const model = requireNonEmptyString(requested.model, "execution.requested.model");
  if (requested.profile !== null && typeof requested.profile !== "string") {
    reject("execution.requested.profile must be a string or null");
  }
  if (requested.profile !== null && requested.profile.length === 0) {
    reject("execution.requested.profile must be non-empty when present");
  }
  if (
    requested.reasoning_effort !== "low" &&
    requested.reasoning_effort !== "medium" &&
    requested.reasoning_effort !== "high"
  ) {
    reject("execution.requested.reasoning_effort is invalid");
  }

  let placement: InvocationExecution["placement"];
  if ("placement" in execution) {
    if (
      execution.placement !== "local_microvm" &&
      execution.placement !== "managed_container"
    ) {
      reject("execution.placement is invalid");
    }
    placement = execution.placement;
  }
  const contextRendererRevision = requireNonEmptyString(
    execution.context_renderer_revision,
    "execution.context_renderer_revision",
  );
  let verifierRef: string | undefined;
  if ("verifier_ref" in execution) {
    if (typeof execution.verifier_ref !== "string") {
      reject("execution.verifier_ref must be a string when present");
    }
    verifierRef = execution.verifier_ref;
  }

  return {
    transport: {
      harness: transport.harness,
      transport: transportName,
      harness_version: harnessVersion,
      adapter_revision: adapterRevision,
    },
    requested: {
      provider,
      model,
      profile: requested.profile,
      reasoning_effort: requested.reasoning_effort,
    },
    ...(placement === undefined ? {} : { placement }),
    context_renderer_revision: contextRendererRevision,
    ...(verifierRef === undefined ? {} : { verifier_ref: verifierRef }),
  };
}

function validateOutputFormat(value: JsonValue | undefined): JsonSchemaOutputFormat {
  const outputFormat = requireObject(value, "output_format");
  assertExactKeys(
    outputFormat,
    ["type", "schema", "retry_count"],
    "output_format",
  );
  if (outputFormat.type !== "json_schema") {
    reject("output_format.type must be 'json_schema'");
  }
  const schema = requireObject(outputFormat.schema, "output_format.schema");
  validateJsonObjectValues(schema, "output_format.schema");
  if (outputFormat.retry_count !== 2) {
    reject("output_format.retry_count must be 2");
  }
  return {
    type: "json_schema",
    schema,
    retry_count: 2,
  };
}

function requireDigest(value: JsonValue | undefined, path: string): Sha256Digest {
  if (typeof value !== "string" || !/^sha256:[0-9a-f]{64}$/.test(value)) {
    reject(`${path} must match sha256:<64 lowercase hex>`);
  }
  return value as Sha256Digest;
}

function requireContractVersion(value: JsonValue | undefined): void {
  if (value !== "pillbox.execution/2") {
    reject("contract_version must be 'pillbox.execution/2'");
  }
}

function requireNonNegativeInteger(
  value: JsonValue | undefined,
  path: string,
): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    reject(`${path} must be a non-negative safe integer`);
  }
  return value;
}

function requirePositiveInteger(value: JsonValue | undefined, path: string): number {
  const number = requireNonNegativeInteger(value, path);
  if (number === 0) reject(`${path} must be greater than zero`);
  return number;
}

function requireNonEmptyString(value: JsonValue | undefined, path: string): string {
  if (typeof value !== "string" || value.length === 0) {
    reject(`${path} must be a non-empty string`);
  }
  return value;
}

function requireObject(
  value: JsonValue | undefined,
  path: string,
): Record<string, JsonValue> {
  if (!isJsonObject(value)) reject(`${path} must be an object`);
  return value;
}

function assertExactKeys(
  value: Record<string, JsonValue>,
  allowed: readonly string[],
  path: string,
): void {
  const allowedKeys = new Set(allowed);
  const unknown = Object.keys(value).find((key) => !allowedKeys.has(key));
  if (unknown !== undefined) reject(`${path}.${unknown} is not allowed`);
}

function validateJsonObjectValues(
  value: Record<string, JsonValue>,
  path: string,
): void {
  for (const [key, item] of Object.entries(value)) {
    assertJsonValue(item, `${path}.${key}`);
  }
}

function assertJsonValue(
  value: unknown,
  path: string,
  ancestors = new Set<object>(),
): asserts value is JsonValue {
  if (value === null || typeof value === "boolean" || typeof value === "string") return;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) reject(`${path} contains a non-finite number`);
    return;
  }
  if (typeof value !== "object") {
    reject(`${path} contains a non-JSON value`);
  }
  if (ancestors.has(value)) reject(`${path} contains a cyclic value`);
  if (Object.getOwnPropertySymbols(value).length > 0) {
    reject(`${path} contains symbol keys`);
  }
  if (!Array.isArray(value) && !isJsonObject(value)) {
    reject(`${path} contains a non-JSON object`);
  }
  ancestors.add(value);
  try {
    if (Array.isArray(value)) {
      for (let index = 0; index < value.length; index++) {
        if (!Object.prototype.hasOwnProperty.call(value, index)) {
          reject(`${path}[${index}] is missing`);
        }
        assertJsonValue(value[index], `${path}[${index}]`, ancestors);
      }
      return;
    }
    for (const [key, item] of Object.entries(value)) {
      assertJsonValue(item, `${path}.${key}`, ancestors);
    }
  } finally {
    ancestors.delete(value);
  }
}

function isJsonObject(value: unknown): value is Record<string, JsonValue> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

/** Byte-for-byte mirror of the repository's canonicalJson helper. */
function canonicalJson(value: JsonValue): string {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) reject("JSON numbers must be finite");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

async function sha256Json(value: JsonValue): Promise<Sha256Digest> {
  return sha256Digest(canonicalJson(value));
}

async function sha256Digest(value: string): Promise<Sha256Digest> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(value),
  );
  const hex = [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  return `sha256:${hex}`;
}

function reject(message: string): never {
  throw new ExecutionBoundaryError(message);
}
