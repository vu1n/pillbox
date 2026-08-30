import {
  canonicalJson,
  type ExecuteInvocationV2Request,
  type ExecuteInvocationV2Result,
  type InvocationExecution,
  type JsonSchemaOutputFormat,
  type JsonValue,
} from "./codex_execution.js";
import { sha256Hex } from "./runtime_identity.js";

/** Compatibility contract for Huddles callers that predate pillbox.execution/2. */
export interface CanonicalSessionRequest {
  readonly requested_model: string;
  readonly [key: string]: JsonValue;
}

export interface EnsureSessionRequest {
  readonly workspace_id: string;
  readonly effect_id: string;
  readonly canonical_request: CanonicalSessionRequest;
}

export interface SessionRef {
  readonly session_id: string;
  /** Worker RPC widens tuples to arrays; callers validate the two-position contract. */
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
  readonly session_ref: SessionRef;
  readonly delivery_receipt_id: string;
  readonly rendered_input: string;
  readonly rendered_input_hash: string;
  readonly tool_policy: "deny_all";
  readonly harness: "opencode";
  readonly requested_model: string;
  readonly output_format: JsonSchemaOutputFormat;
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
  };
}

/** Preserve the historical raw-hex name while sharing canonical JSON encoding. */
export async function deriveExecutionSessionName(
  workspaceId: string,
  effectId: string,
): Promise<string> {
  return `ensure-${await sha256Hex(canonicalJson([workspaceId, effectId]))}`;
}

/** Stateless compatibility projection; generic execution owns idempotency. */
export async function ensureLegacySession(value: unknown): Promise<EnsureSessionResult> {
  const request = validateEnsureSessionRequest(value);
  return {
    session_ref: {
      session_id: await deriveExecutionSessionName(
        request.workspace_id,
        request.effect_id,
      ),
    },
    disposition: "reused",
    attribution: {
      requested_model: request.canonical_request.requested_model,
      served_model: null,
      status: "unavailable",
    },
  };
}

export async function invokeLegacySession(
  value: unknown,
  execute: (request: ExecuteInvocationV2Request) => Promise<ExecuteInvocationV2Result>,
): Promise<InvokeSessionResult> {
  const request = await validateInvokeSessionRequest(value);
  const expectedSessionId = await deriveExecutionSessionName(
    request.workspace_id,
    request.effect_id,
  );
  if (request.session_ref.session_id !== expectedSessionId) {
    throw new Error("invoke request does not match its deterministic session");
  }
  const result = await execute(legacyExecutionRequest(request));
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
): ExecuteInvocationV2Request {
  const slash = request.requested_model.indexOf("/");
  const provider = slash > 0 ? request.requested_model.slice(0, slash) : "custom";
  const model =
    slash > 0 ? request.requested_model.slice(slash + 1) : request.requested_model;
  const execution: InvocationExecution = {
    transport: {
      harness: "opencode",
      transport: "http",
      harness_version: "legacy",
      adapter_revision: "huddles-compat/1",
    },
    requested: {
      provider,
      model,
      profile: null,
      reasoning_effort: "medium",
    },
    placement: "managed_container",
    context_renderer_revision: "huddles-compat/1",
  };
  return {
    contract_version: "pillbox.execution/2",
    session_ref: request.session_ref,
    invocation_id: request.invocation_id,
    idempotency_key: request.delivery_receipt_id,
    rendered_input: request.rendered_input,
    rendered_input_hash: request.rendered_input_hash as `sha256:${string}`,
    tool_policy: request.tool_policy,
    execution,
    execution_policy_revision: "huddles-compat/1",
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
  const requestedModel = requireIdentifier(value.requested_model, "requested_model");
  if (value.harness !== "opencode") {
    throw new EnsureSessionRequestError("invoke request harness must be 'opencode'");
  }
  if (value.tool_policy !== "deny_all") {
    throw new EnsureSessionRequestError(
      "invoke request tool_policy must be 'deny_all'",
    );
  }
  if (!isJsonObject(value.output_format)) {
    throw new EnsureSessionRequestError("output_format must be an object");
  }
  if (value.output_format.type !== "json_schema") {
    throw new EnsureSessionRequestError("output_format.type must be 'json_schema'");
  }
  if (!isJsonObject(value.output_format.schema)) {
    throw new EnsureSessionRequestError(
      "output_format.schema must be a JSON object",
    );
  }
  validateJsonValue(value.output_format.schema, "output_format.schema");
  if (value.output_format.retry_count !== 2) {
    throw new EnsureSessionRequestError("output_format.retry_count must be 2");
  }
  if (!isJsonObject(value.session_ref)) {
    throw new EnsureSessionRequestError("session_ref must be an object");
  }
  const sessionId = requireIdentifier(
    value.session_ref.session_id,
    "session_ref.session_id",
  );
  const expectedHash = `sha256:${await sha256Hex(renderedInput)}`;
  if (renderedInputHash !== expectedHash) {
    throw new EnsureSessionRequestError(
      "rendered_input_hash does not match rendered_input",
    );
  }
  return {
    workspace_id: workspaceId,
    effect_id: effectId,
    invocation_id: invocationId,
    session_ref: { session_id: sessionId },
    delivery_receipt_id: deliveryReceiptId,
    rendered_input: renderedInput,
    rendered_input_hash: renderedInputHash,
    tool_policy: value.tool_policy,
    harness: value.harness,
    requested_model: requestedModel,
    output_format: {
      type: value.output_format.type,
      schema: value.output_format.schema,
      retry_count: value.output_format.retry_count,
    },
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
  ]);
  return {
    code: supported.has(code)
      ? (code as
          | "runtime_unavailable"
          | "runtime_failed"
          | "runtime_interrupted"
          | "structured_output_missing")
      : "runtime_failed",
    message,
  };
}

function validateCanonicalRequest(
  value: Record<string, JsonValue>,
): CanonicalSessionRequest {
  const requestedModel = value.requested_model;
  if (typeof requestedModel !== "string" || requestedModel.length === 0) {
    throw new EnsureSessionRequestError(
      "canonical_request.requested_model must be a non-empty string",
    );
  }
  validateJsonValue(value, "canonical_request");
  return value as CanonicalSessionRequest;
}

function requireIdentifier(value: JsonValue | undefined, field: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new EnsureSessionRequestError(`${field} must be a non-empty string`);
  }
  return value;
}

function validateJsonValue(value: unknown, path: string): asserts value is JsonValue {
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return;
  }
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
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}
