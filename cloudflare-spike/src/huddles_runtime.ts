import { WorkerEntrypoint } from "cloudflare:workers";
import { sha256Hex } from "./runtime_identity.js";
import type { Env } from "./worker.js";
export { isHuddlesSessionName } from "./huddles_policy.js";
export { deriveSandboxRuntimeId, sha256Hex } from "./runtime_identity.js";

export type JsonPrimitive = null | boolean | number | string;
export type JsonValue =
  | JsonPrimitive
  | JsonValue[]
  | { readonly [key: string]: JsonValue };

/**
 * Huddles owns the meaning of this object. Pillbox only requires the requested
 * model so it can return an explicit attribution outcome without guessing.
 */
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

export interface JsonSchemaOutputFormat {
  readonly type: "json_schema";
  readonly schema: { readonly [key: string]: JsonValue };
  readonly retry_count: 2;
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

/** Validate the serializable boundary before data reaches a Worker RPC or DO. */
export function validateEnsureSessionRequest(
  value: unknown,
): EnsureSessionRequest {
  if (!isJsonObject(value)) {
    throw new EnsureSessionRequestError("ensure request must be an object");
  }

  const workspaceId = requireIdentifier(value.workspace_id, "workspace_id");
  const effectId = requireIdentifier(value.effect_id, "effect_id");
  if (!isJsonObject(value.canonical_request)) {
    throw new EnsureSessionRequestError("canonical_request must be an object");
  }
  const canonicalRequest = validateCanonicalRequest(value.canonical_request);
  return {
    workspace_id: workspaceId,
    effect_id: effectId,
    canonical_request: canonicalRequest,
  };
}

/** Deterministic canonical JSON for the JSON subset accepted above. */
export function canonicalJson(value: JsonValue): string {
  if (
    value === null ||
    typeof value === "boolean" ||
    typeof value === "string"
  ) {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value))
      throw new EnsureSessionRequestError("JSON numbers must be finite");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

/** A canonical tuple keeps arbitrary opaque IDs from creating ambiguous names. */
export async function deriveSessionGatewayName(
  workspaceId: string,
  effectId: string,
): Promise<string> {
  return `ensure-${await sha256Hex(canonicalJson([workspaceId, effectId]))}`;
}

/**
 * Private same-account RPC surface for Huddles. There is deliberately no HTTP
 * ensure route: WorkerEntrypoint methods are reachable only through a service
 * binding, while durable binding state belongs to SessionGateway.
 */
export class HuddlesRuntimeEntrypoint extends WorkerEntrypoint<Env> {
  async ensureSession(
    request: EnsureSessionRequest,
  ): Promise<EnsureSessionResult> {
    const validated = validateEnsureSessionRequest(request);
    const name = await deriveSessionGatewayName(
      validated.workspace_id,
      validated.effect_id,
    );
    const id = this.env.SessionGateway.idFromName(name);
    const stub = this.env.SessionGateway.get(id);
    setAgentName(stub, name);
    return stub.ensureSession(validated);
  }

  async invokeSession(
    request: InvokeSessionRequest,
  ): Promise<InvokeSessionResult> {
    const validated = await validateInvokeSessionRequest(request);
    const name = await deriveSessionGatewayName(
      validated.workspace_id,
      validated.effect_id,
    );
    const id = this.env.SessionGateway.idFromName(name);
    const stub = this.env.SessionGateway.get(id);
    setAgentName(stub, name);
    return stub.invokeSession(validated);
  }

  async fetch(): Promise<Response> {
    return new Response("not found\n", { status: 404 });
  }
}

function setAgentName(stub: object, name: string): void {
  const maybeNamed = stub as { setName?: (value: string) => void };
  maybeNamed.setName?.(name);
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
  const renderedInput = requireIdentifier(
    value.rendered_input,
    "rendered_input",
  );
  const renderedInputHash = requireIdentifier(
    value.rendered_input_hash,
    "rendered_input_hash",
  );
  const requestedModel = requireIdentifier(
    value.requested_model,
    "requested_model",
  );
  if (value.harness !== "opencode") {
    throw new EnsureSessionRequestError(
      "invoke request harness must be 'opencode'",
    );
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
    throw new EnsureSessionRequestError(
      "output_format.type must be 'json_schema'",
    );
  }
  if (!isJsonObject(value.output_format.schema)) {
    throw new EnsureSessionRequestError(
      "output_format.schema must be a JSON object",
    );
  }
  validateJsonValue(value.output_format.schema, "output_format.schema");
  if (value.output_format.retry_count !== 2) {
    throw new EnsureSessionRequestError(
      "output_format.retry_count must be 2",
    );
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

function requireIdentifier(
  value: JsonValue | undefined,
  field: string,
): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new EnsureSessionRequestError(`${field} must be a non-empty string`);
  }
  return value;
}

function validateJsonValue(
  value: unknown,
  path: string,
): asserts value is JsonValue {
  if (value === null || typeof value === "boolean" || typeof value === "string")
    return;
  if (typeof value === "number") {
    if (!Number.isFinite(value))
      throw new EnsureSessionRequestError(
        `${path} contains a non-finite number`,
      );
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((item, index) =>
      validateJsonValue(item, `${path}[${index}]`),
    );
    return;
  }
  if (isJsonObject(value)) {
    for (const [key, item] of Object.entries(value))
      validateJsonValue(item, `${path}.${key}`);
    return;
  }
  throw new EnsureSessionRequestError(`${path} contains a non-JSON value`);
}

function isJsonObject(value: unknown): value is Record<string, JsonValue> {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}
