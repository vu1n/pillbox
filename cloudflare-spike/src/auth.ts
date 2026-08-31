const enc = new TextEncoder();

export const MANAGED_CAPABILITY_AUDIENCE = "pillbox-managed" as const;
export const MANAGED_CAPABILITY_VERSION = 1 as const;
export const MAX_MANAGED_CAPABILITY_LIFETIME_MS = 15 * 60 * 1000;

export type ManagedOperation =
  | "execute"
  | "status"
  | "cancel"
  | "workspace_provision"
  | "workspace_finalize";

export interface ManagedCapability {
  readonly version: typeof MANAGED_CAPABILITY_VERSION;
  readonly subject: string;
  readonly audience: typeof MANAGED_CAPABILITY_AUDIENCE;
  readonly expires_at_ms: number;
  readonly operation: ManagedOperation;
  readonly request_sha256: `sha256:${string}`;
  readonly session_id?: string;
  readonly invocation_id?: string;
}

export interface CapabilityScope {
  readonly operation: ManagedOperation;
  readonly request_sha256: `sha256:${string}`;
  readonly session_id?: string;
  readonly invocation_id?: string;
}

const OPERATIONS = new Set<ManagedOperation>([
  "execute",
  "status",
  "cancel",
  "workspace_provision",
  "workspace_finalize",
]);

async function hmacKey(secret: string): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"],
  );
}

function b64urlEncode(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function b64urlDecode(value: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]+$/.test(value)) throw new Error("invalid base64url");
  const pad = value.length % 4 ? "=".repeat(4 - (value.length % 4)) : "";
  const decoded = atob(value.replace(/-/g, "+").replace(/_/g, "/") + pad);
  return Uint8Array.from(decoded, (character) => character.charCodeAt(0));
}

export async function signManagedCapability(
  capability: ManagedCapability,
  secret: string,
): Promise<string> {
  const claim = b64urlEncode(enc.encode(JSON.stringify(capability)));
  const key = await hmacKey(secret);
  const signature = new Uint8Array(
    await crypto.subtle.sign("HMAC", key, enc.encode(claim)),
  );
  return `${claim}.${b64urlEncode(signature)}`;
}

export async function verifyManagedCapability(
  token: string,
  secret: string,
  expected: CapabilityScope,
  now = Date.now(),
): Promise<ManagedCapability | null> {
  const parts = token.split(".");
  if (parts.length !== 2 || parts[0].length === 0 || parts[1].length === 0) {
    return null;
  }
  try {
    const key = await hmacKey(secret);
    const validSignature = await crypto.subtle.verify(
      "HMAC",
      key,
      b64urlDecode(parts[1]),
      enc.encode(parts[0]),
    );
    if (!validSignature) return null;
    const value: unknown = JSON.parse(
      new TextDecoder().decode(b64urlDecode(parts[0])),
    );
    if (!isManagedCapability(value, now)) return null;
    if (value.operation !== expected.operation) return null;
    if (value.request_sha256 !== expected.request_sha256) return null;
    if (value.session_id !== expected.session_id) return null;
    if (value.invocation_id !== expected.invocation_id) return null;
    return value;
  } catch {
    return null;
  }
}

function isManagedCapability(
  value: unknown,
  now: number,
): value is ManagedCapability {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const claim = value as Record<string, unknown>;
  const allowed = new Set([
    "version",
    "subject",
    "audience",
    "expires_at_ms",
    "operation",
    "request_sha256",
    "session_id",
    "invocation_id",
  ]);
  if (Object.keys(claim).some((key) => !allowed.has(key))) return false;
  if (
    claim.version !== MANAGED_CAPABILITY_VERSION ||
    claim.audience !== MANAGED_CAPABILITY_AUDIENCE ||
    typeof claim.subject !== "string" ||
    claim.subject.length === 0 ||
    claim.subject.length > 256 ||
    typeof claim.expires_at_ms !== "number" ||
    !Number.isSafeInteger(claim.expires_at_ms) ||
    claim.expires_at_ms <= now ||
    claim.expires_at_ms > now + MAX_MANAGED_CAPABILITY_LIFETIME_MS ||
    typeof claim.operation !== "string" ||
    !OPERATIONS.has(claim.operation as ManagedOperation) ||
    typeof claim.request_sha256 !== "string" ||
    !/^sha256:[0-9a-f]{64}$/.test(claim.request_sha256)
  ) {
    return false;
  }
  if (
    (claim.session_id !== undefined &&
      (typeof claim.session_id !== "string" ||
        claim.session_id.length === 0 ||
        claim.session_id.length > 128)) ||
    (claim.invocation_id !== undefined &&
      (typeof claim.invocation_id !== "string" ||
        claim.invocation_id.length === 0 ||
        claim.invocation_id.length > 128))
  ) {
    return false;
  }
  return true;
}

export function bearerToken(req: Request): string | null {
  const header = req.headers.get("authorization") ?? "";
  const match = /^Bearer\s+([^\s]+)$/i.exec(header);
  return match ? match[1] : null;
}
