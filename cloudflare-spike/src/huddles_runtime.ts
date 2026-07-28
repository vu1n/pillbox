import { WorkerEntrypoint } from "cloudflare:workers";
import type { Env } from "./worker.js";

export type JsonPrimitive = null | boolean | number | string;
export type JsonValue = JsonPrimitive | JsonValue[] | { readonly [key: string]: JsonValue };

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

export class EnsureSessionRequestError extends Error {
  readonly code = "invalid_ensure_session_request" as const;

  constructor(message: string) {
    super(message);
    this.name = "EnsureSessionRequestError";
  }
}

/** Validate the serializable boundary before data reaches a Worker RPC or DO. */
export function validateEnsureSessionRequest(value: unknown): EnsureSessionRequest {
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
  if (value === null || typeof value === "boolean" || typeof value === "string") {
    return JSON.stringify(value);
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new EnsureSessionRequestError("JSON numbers must be finite");
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

export async function sha256Hex(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/** A canonical tuple keeps arbitrary opaque IDs from creating ambiguous names. */
export async function deriveSessionGatewayName(workspaceId: string, effectId: string): Promise<string> {
  return `ensure-${await sha256Hex(canonicalJson([workspaceId, effectId]))}`;
}

/**
 * Private same-account RPC surface for Huddles. There is deliberately no HTTP
 * ensure route: WorkerEntrypoint methods are reachable only through a service
 * binding, while durable binding state belongs to SessionGateway.
 */
export class HuddlesRuntimeEntrypoint extends WorkerEntrypoint<Env> {
  async ensureSession(request: EnsureSessionRequest): Promise<EnsureSessionResult> {
    const validated = validateEnsureSessionRequest(request);
    const name = await deriveSessionGatewayName(validated.workspace_id, validated.effect_id);
    const id = this.env.SessionGateway.idFromName(name);
    return this.env.SessionGateway.get(id).ensureSession(validated);
  }

  async fetch(): Promise<Response> {
    return new Response("not found\n", { status: 404 });
  }
}

function validateCanonicalRequest(value: Record<string, JsonValue>): CanonicalSessionRequest {
  const requestedModel = value.requested_model;
  if (typeof requestedModel !== "string" || requestedModel.length === 0) {
    throw new EnsureSessionRequestError("canonical_request.requested_model must be a non-empty string");
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
  if (value === null || typeof value === "boolean" || typeof value === "string") return;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new EnsureSessionRequestError(`${path} contains a non-finite number`);
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((item, index) => validateJsonValue(item, `${path}[${index}]`));
    return;
  }
  if (isJsonObject(value)) {
    for (const [key, item] of Object.entries(value)) validateJsonValue(item, `${path}.${key}`);
    return;
  }
  throw new EnsureSessionRequestError(`${path} contains a non-JSON value`);
}

function isJsonObject(value: unknown): value is Record<string, JsonValue> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}
