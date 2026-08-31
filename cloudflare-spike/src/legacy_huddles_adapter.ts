import {
  canonicalJson,
  type ExecuteInvocationV2Request,
  type ExecuteInvocationV2Result,
  type InvocationExecution,
  type JsonSchemaOutputFormat,
  type JsonValue,
} from "./codex_execution.js";
import {
  validateManagedRequestBinding,
  validateSignedExecutionGrant,
  type PillboxManagedAuthorization,
} from "./managed_contract.js";
import { sha256Hex } from "./runtime_identity.js";

/** Compatibility contract for Huddles callers that predate pillbox.execution/2. */
export interface CanonicalSessionRequest {
  readonly requested_model?: string;
  readonly execution?: JsonValue;
  readonly [key: string]: JsonValue | undefined;
}

export interface EnsureSessionRequest {
  readonly workspace_id: string;
  readonly effect_id: string;
  readonly canonical_request: CanonicalSessionRequest;
  readonly managed_authorization?: PillboxManagedAuthorization;
}

export interface SessionRef {
  readonly session_id: string;
  readonly realm?: {
    readonly runtime: "pillbox";
    readonly execution_realm_id: string;
  };
  readonly seq_range?: readonly number[];
}

export interface EffectCompletionAttribution {
  readonly requested_model: string;
  readonly served_model: null;
  readonly status: "unavailable";
}

export interface EnsureSessionResponse {
  readonly session_ref: SessionRef;
  readonly disposition: "created" | "reused";
  readonly attribution: EffectCompletionAttribution;
}

export interface EnsureSessionConflict {
  readonly code: "ensure_session_conflict";
  readonly workspace_id: string;
  readonly effect_id: string;
  readonly existing_request_hash: string;
  readonly requested_request_hash: string;
}

export type EnsureSessionResult = EnsureSessionResponse | EnsureSessionConflict;

export interface InvokeSessionRequest {
  readonly workspace_id: string;
  readonly effect_id: string;
  readonly invocation_id: string;
  readonly activity_principal_id?: string;
  readonly policy_id?: string;
  readonly run_id?: string;
  readonly packet_id?: string;
  readonly session_ref: SessionRef;
  readonly delivery_receipt_id: string;
  readonly rendered_input: string;
  readonly rendered_input_hash: string;
  readonly tool_policy: "deny_all";
  readonly harness?: "opencode";
  readonly requested_model: string;
  readonly execution?: JsonValue;
  readonly execution_policy_revision?: string;
  readonly output_format: JsonSchemaOutputFormat;
  readonly managed_authorization?: PillboxManagedAuthorization;
}

export type InvokeSessionResult =
  | {
      readonly status: "completed";
      readonly disposition: "created" | "reused";
      readonly session_ref: SessionRef;
      readonly output_text: string;
    }
  | {
      readonly status: "failed";
      readonly disposition: "created" | "reused";
      readonly session_ref: SessionRef;
      readonly error: {
        readonly code:
          | "runtime_unavailable"
          | "unsupported_execution"
          | "runtime_failed"
          | "runtime_interrupted"
          | "provider_failed"
          | "structured_output_missing";
        readonly message: string;
      };
    }
  | {
      readonly status: "running";
      readonly disposition: "reused";
      readonly session_ref: SessionRef;
    }
  | {
      readonly code: "invoke_session_conflict";
      readonly workspace_id: string;
      readonly effect_id: string;
      readonly invocation_id: string;
      readonly existing_request_hash: string;
      readonly requested_request_hash: string;
    };

export class EnsureSessionRequestError extends Error {
  readonly code = "invalid_ensure_session_request" as const;

  constructor(message: string) {
    super(message);
    this.name = "EnsureSessionRequestError";
  }
}

export function validateEnsureSessionRequest(value: unknown): EnsureSessionRequest {
  if (!isJsonObject(value)) {
    throw new EnsureSessionRequestError("ensure request must be an object");
  }
  const workspaceId = requireIdentifier(value.workspace_id, "workspace_id");
  const effectId = requireIdentifier(value.effect_id, "effect_id");
  if (!isJsonObject(value.canonical_request)) {
    throw new EnsureSessionRequestError("canonical_request must be an object");
  }
  return {
    workspace_id: workspaceId,
    effect_id: effectId,
    canonical_request: validateCanonicalRequest(value.canonical_request),
    ...(value.managed_authorization === undefined
      ? {}
      : { managed_authorization: validateManagedAuthorization(value.managed_authorization) }),
  };
}

export async function deriveExecutionSessionName(
  workspaceId: string,
  effectId: string,
): Promise<string> {
  return `ensure-${await sha256Hex(canonicalJson([workspaceId, effectId]))}`;
}

/** Stateless compatibility projection; generic execution owns idempotency. */
export async function ensureLegacySession(
  value: unknown,
  executionRealmId?: string,
): Promise<EnsureSessionResult> {
  const request = validateEnsureSessionRequest(value);
  return {
    session_ref: {
      session_id: await deriveExecutionSessionName(request.workspace_id, request.effect_id),
      ...(executionRealmId === undefined
        ? {}
        : {
            realm: {
              runtime: "pillbox" as const,
              execution_realm_id: executionRealmId,
            },
          }),
    },
    disposition: "reused",
    attribution: {
      requested_model: requestedModelFromCanonicalRequest(request.canonical_request),
      served_model: null,
      status: "unavailable",
    },
  };
}

export async function invokeLegacySession(
  value: unknown,
  execute: (request: ExecuteInvocationV2Request) => Promise<ExecuteInvocationV2Result>,
  controllerContextHash?: `sha256:${string}`,
): Promise<InvokeSessionResult> {
  const request = await validateInvokeSessionRequest(value);
  const expectedSessionId = await deriveExecutionSessionName(
    request.workspace_id,
    request.effect_id,
  );
  if (request.session_ref.session_id !== expectedSessionId) {
    throw new Error("invoke request does not match its deterministic session");
  }
  const result = await execute(
    legacyExecutionRequest(request, controllerContextHash),
  );
  if (result.status === "conflict") {
    return {
      code: "invoke_session_conflict",
      workspace_id: request.workspace_id,
      effect_id: request.effect_id,
      invocation_id: request.invocation_id,
      existing_request_hash: result.error.existing_request_hash,
      requested_request_hash: result.error.requested_request_hash,
    };
  }
  const session_ref: SessionRef = {
    session_id: result.session_ref.session_id,
    ...(request.session_ref.realm === undefined ? {} : { realm: request.session_ref.realm }),
    ...(result.session_ref.seq_range === undefined
      ? {}
      : { seq_range: result.session_ref.seq_range }),
  };
  if (result.status === "running") {
    return { status: "running", disposition: "reused", session_ref };
  }
  if (result.status === "completed") {
    return {
      status: "completed",
      disposition: result.disposition,
      session_ref,
      output_text:
        result.output.text ??
        (result.output.json === undefined ? "" : JSON.stringify(result.output.json)),
    };
  }
  return {
    status: "failed",
    disposition: result.disposition,
    session_ref,
    error: legacyError(result.error.code, result.error.message),
  };
}

export function legacyExecutionRequest(
  request: InvokeSessionRequest,
  controllerContextHash?: `sha256:${string}`,
): ExecuteInvocationV2Request {
  return {
    contract_version: "pillbox.execution/2",
    session_ref: { session_id: request.session_ref.session_id },
    invocation_id: request.invocation_id,
    idempotency_key: request.delivery_receipt_id,
    rendered_input: request.rendered_input,
    rendered_input_hash: request.rendered_input_hash as `sha256:${string}`,
    tool_policy: request.tool_policy,
    execution:
      request.execution === undefined
        ? legacyExecution(request.requested_model)
        : managedExecution(request.execution),
    execution_policy_revision:
      request.execution_policy_revision ?? "huddles-compat/1",
    ...(controllerContextHash === undefined
      ? {}
      : { controller_context_hash: controllerContextHash }),
    output_format: request.output_format,
  };
}

export async function validateInvokeSessionRequest(
  value: unknown,
): Promise<InvokeSessionRequest> {
  if (!isJsonObject(value)) {
    throw new EnsureSessionRequestError("invoke request must be an object");
  }
  const workspaceId = requireIdentifier(value.workspace_id, "workspace_id");
  const effectId = requireIdentifier(value.effect_id, "effect_id");
  const invocationId = requireIdentifier(value.invocation_id, "invocation_id");
  const deliveryReceiptId = requireIdentifier(
    value.delivery_receipt_id,
    "delivery_receipt_id",
  );
  const renderedInput = requireIdentifier(value.rendered_input, "rendered_input");
  const renderedInputHash = requireIdentifier(
    value.rendered_input_hash,
    "rendered_input_hash",
  );
  const legacyHarness = value.harness === "opencode";
  if (!legacyHarness && !isJsonObject(value.execution)) {
    throw new EnsureSessionRequestError("invoke request execution is required");
  }
  const requestedModel = legacyHarness
    ? requireIdentifier(value.requested_model, "requested_model")
    : requestedModelFromExecution(value.execution);
  if (value.tool_policy !== "deny_all") {
    throw new EnsureSessionRequestError("invoke request tool_policy must be 'deny_all'");
  }
  const outputFormat = validateOutputFormat(value.output_format);
  if (!isJsonObject(value.session_ref)) {
    throw new EnsureSessionRequestError("session_ref must be an object");
  }
  const sessionId = requireIdentifier(
    value.session_ref.session_id,
    "session_ref.session_id",
  );
  const expectedHash = `sha256:${await sha256Hex(renderedInput)}`;
  if (renderedInputHash !== expectedHash) {
    throw new EnsureSessionRequestError("rendered_input_hash does not match rendered_input");
  }
  const activityPrincipalId = optionalIdentifier(
    value.activity_principal_id,
    "activity_principal_id",
  );
  const policyId = optionalIdentifier(value.policy_id, "policy_id");
  const runId = optionalIdentifier(value.run_id, "run_id");
  const packetId = optionalIdentifier(value.packet_id, "packet_id");
  const executionPolicyRevision = optionalIdentifier(
    value.execution_policy_revision,
    "execution_policy_revision",
  );
  const managedAuthorization =
    value.managed_authorization === undefined
      ? undefined
      : validateManagedAuthorization(value.managed_authorization);
  if (managedAuthorization !== undefined) {
    const missing = [
      ["activity_principal_id", activityPrincipalId],
      ["policy_id", policyId],
      ["run_id", runId],
      ["packet_id", packetId],
      ["execution_policy_revision", executionPolicyRevision],
      ["execution", value.execution],
    ].find(([, fieldValue]) => fieldValue === undefined)?.[0];
    if (missing !== undefined) {
      throw new EnsureSessionRequestError(
        `managed invoke request requires ${missing}`,
      );
    }
  }
  return {
    workspace_id: workspaceId,
    effect_id: effectId,
    invocation_id: invocationId,
    ...(activityPrincipalId === undefined ? {} : { activity_principal_id: activityPrincipalId }),
    ...(policyId === undefined ? {} : { policy_id: policyId }),
    ...(runId === undefined ? {} : { run_id: runId }),
    ...(packetId === undefined ? {} : { packet_id: packetId }),
    session_ref: validateSessionRefForRequest(value.session_ref, sessionId),
    delivery_receipt_id: deliveryReceiptId,
    rendered_input: renderedInput,
    rendered_input_hash: renderedInputHash,
    tool_policy: "deny_all",
    ...(legacyHarness ? { harness: "opencode" as const } : {}),
    requested_model: requestedModel,
    ...(value.execution === undefined ? {} : { execution: value.execution }),
    ...(executionPolicyRevision === undefined
      ? {}
      : { execution_policy_revision: executionPolicyRevision }),
    output_format: outputFormat,
    ...(managedAuthorization === undefined
      ? {}
      : { managed_authorization: managedAuthorization }),
  };
}

function legacyExecution(requestedModel: string): InvocationExecution {
  const slash = requestedModel.indexOf("/");
  const provider = slash > 0 ? requestedModel.slice(0, slash) : "custom";
  const model = slash > 0 ? requestedModel.slice(slash + 1) : requestedModel;
  return {
    transport: {
      harness: "opencode",
      transport: "http",
      harness_version: "legacy",
      adapter_revision: "huddles-compat/1",
    },
    requested: { provider, model, profile: null, reasoning_effort: "medium" },
    placement: "managed_container",
    context_renderer_revision: "huddles-compat/1",
  };
}

/** Convert the signed Huddles execution identity into the generic runtime shape. */
function managedExecution(value: JsonValue): InvocationExecution {
  if (!isJsonObject(value) || !isJsonObject(value.requested) || !isJsonObject(value.transport)) {
    throw new EnsureSessionRequestError(
      "execution must contain requested and transport objects",
    );
  }
  if (
    value.placement !== "managed_container" ||
    value.transport.harness !== "opencode" ||
    value.transport.transport !== "cloudflare-service-binding"
  ) {
    throw new EnsureSessionRequestError(
      "managed execution must use opencode/cloudflare-service-binding",
    );
  }
  rejectUnknownFields(
    value,
    [
      "placement",
      "requested",
      "transport",
      "context_renderer_revision",
      "verifier_ref",
    ],
    "execution",
  );
  rejectUnknownFields(
    value.requested,
    ["provider", "model", "profile", "reasoning_effort"],
    "execution.requested",
  );
  rejectUnknownFields(
    value.transport,
    ["harness", "transport", "harness_version", "adapter_revision"],
    "execution.transport",
  );
  const profile = value.requested.profile;
  if (profile !== undefined && profile !== null && typeof profile !== "string") {
    throw new EnsureSessionRequestError("execution.requested.profile is invalid");
  }
  const effort = value.requested.reasoning_effort ?? "medium";
  if (effort !== "low" && effort !== "medium" && effort !== "high") {
    throw new EnsureSessionRequestError("execution.requested.reasoning_effort is invalid");
  }
  const verifierRef = optionalIdentifier(value.verifier_ref, "execution.verifier_ref");
  return {
    placement: "managed_container",
    requested: {
      provider: requireIdentifier(value.requested.provider, "execution.requested.provider"),
      model: requireIdentifier(value.requested.model, "execution.requested.model"),
      profile: profile ?? null,
      reasoning_effort: effort,
    },
    transport: {
      harness: "opencode",
      transport: "cloudflare-service-binding",
      harness_version: requireIdentifier(
        value.transport.harness_version,
        "execution.transport.harness_version",
      ),
      adapter_revision: requireIdentifier(
        value.transport.adapter_revision,
        "execution.transport.adapter_revision",
      ),
    },
    context_renderer_revision: requireIdentifier(
      value.context_renderer_revision,
      "execution.context_renderer_revision",
    ),
    ...(verifierRef === undefined ? {} : { verifier_ref: verifierRef }),
  };
}

function legacyError(
  code: string,
  message: string,
): Extract<InvokeSessionResult, { readonly status: "failed" }>["error"] {
  const supported = new Set([
    "runtime_failed",
    "runtime_unavailable",
    "runtime_interrupted",
    "structured_output_missing",
    "unsupported_execution",
  ]);
  return {
    code: supported.has(code)
      ? (code as Extract<InvokeSessionResult, { readonly status: "failed" }>["error"]["code"])
      : "runtime_failed",
    message,
  };
}

function validateManagedAuthorization(value: unknown): PillboxManagedAuthorization {
  if (
    !isJsonObject(value) ||
    !isJsonObject(value.grant) ||
    !isJsonObject(value.request_binding)
  ) {
    throw new EnsureSessionRequestError(
      "managed_authorization.grant and request_binding are required objects",
    );
  }
  try {
    return {
      grant: validateSignedExecutionGrant(value.grant),
      request_binding: validateManagedRequestBinding(value.request_binding),
    };
  } catch (cause) {
    throw new EnsureSessionRequestError(
      `managed_authorization is invalid: ${cause instanceof Error ? cause.message : "invalid contract"}`,
    );
  }
}

function validateSessionRefForRequest(
  value: Record<string, JsonValue>,
  sessionId: string,
): SessionRef {
  const realm = value.realm;
  if (realm === undefined) return { session_id: sessionId };
  if (
    !isJsonObject(realm) ||
    realm.runtime !== "pillbox" ||
    typeof realm.execution_realm_id !== "string" ||
    realm.execution_realm_id.length === 0
  ) {
    throw new EnsureSessionRequestError("session_ref.realm is invalid");
  }
  return {
    session_id: sessionId,
    realm: { runtime: "pillbox", execution_realm_id: realm.execution_realm_id },
  };
}

function validateCanonicalRequest(
  value: Record<string, JsonValue>,
): CanonicalSessionRequest {
  if (value.execution !== undefined) {
    const requestedModel = requestedModelFromExecution(value.execution);
    if (
      value.requested_model !== undefined &&
      requireIdentifier(value.requested_model, "canonical_request.requested_model") !==
        requestedModel
    ) {
      throw new EnsureSessionRequestError(
        "canonical_request requested_model does not match execution.requested.model",
      );
    }
  } else if (value.requested_model !== undefined) {
    requireIdentifier(value.requested_model, "canonical_request.requested_model");
  } else {
    throw new EnsureSessionRequestError(
      "canonical_request must contain requested_model or execution",
    );
  }
  validateJsonValue(value, "canonical_request");
  return value as CanonicalSessionRequest;
}

export function requestedModelFromCanonicalRequest(
  request: CanonicalSessionRequest,
): string {
  return request.requested_model ?? requestedModelFromExecution(request.execution);
}

function requestedModelFromExecution(value: JsonValue | undefined): string {
  if (!isJsonObject(value) || !isJsonObject(value.requested) || !isJsonObject(value.transport)) {
    throw new EnsureSessionRequestError(
      "execution must contain requested and transport objects",
    );
  }
  const provider = requireIdentifier(value.requested.provider, "execution.requested.provider");
  const model = requireIdentifier(value.requested.model, "execution.requested.model");
  requireIdentifier(value.transport.harness, "execution.transport.harness");
  requireIdentifier(value.transport.transport, "execution.transport.transport");
  requireIdentifier(value.transport.harness_version, "execution.transport.harness_version");
  requireIdentifier(value.transport.adapter_revision, "execution.transport.adapter_revision");
  requireIdentifier(value.context_renderer_revision, "execution.context_renderer_revision");
  return `${provider}/${model}`;
}

function validateOutputFormat(value: JsonValue | undefined): JsonSchemaOutputFormat {
  if (!isJsonObject(value) || value.type !== "json_schema") {
    throw new EnsureSessionRequestError("output_format.type must be 'json_schema'");
  }
  if (!isJsonObject(value.schema)) {
    throw new EnsureSessionRequestError("output_format.schema must be an object");
  }
  validateJsonValue(value.schema, "output_format.schema");
  if (value.retry_count !== 2) {
    throw new EnsureSessionRequestError("output_format.retry_count must be 2");
  }
  return { type: "json_schema", schema: value.schema, retry_count: 2 };
}

function optionalIdentifier(
  value: JsonValue | undefined,
  field: string,
): string | undefined {
  return value === undefined ? undefined : requireIdentifier(value, field);
}

function rejectUnknownFields(
  value: Record<string, JsonValue>,
  allowed: readonly string[],
  path: string,
): void {
  const known = new Set(allowed);
  const unknown = Object.keys(value).find((key) => !known.has(key));
  if (unknown !== undefined) {
    throw new EnsureSessionRequestError(
      `${path} contains unrecognized field '${unknown}'`,
    );
  }
}

function requireIdentifier(value: JsonValue | undefined, field: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new EnsureSessionRequestError(`${field} must be a non-empty string`);
  }
  return value;
}

function validateJsonValue(value: unknown, path: string): asserts value is JsonValue {
  if (value === null || typeof value === "boolean" || typeof value === "string") return;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw new EnsureSessionRequestError(`${path} contains a non-finite number`);
    }
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((item, index) => validateJsonValue(item, `${path}[${index}]`));
    return;
  }
  if (isJsonObject(value)) {
    for (const [key, item] of Object.entries(value)) {
      validateJsonValue(item, `${path}.${key}`);
    }
    return;
  }
  throw new EnsureSessionRequestError(`${path} contains a non-JSON value`);
}

function isJsonObject(value: unknown): value is Record<string, JsonValue> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}
