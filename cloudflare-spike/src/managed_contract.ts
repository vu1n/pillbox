/**
 * Wire contracts for the supported managed Pillbox placement.
 *
 * This is intentionally dependency-free: the Huddles contract package is
 * deployed by a different Worker. Keep this file in lockstep with
 * app/contracts/src/pillbox-integration.ts; it contains no provider names or
 * credential material.
 */
export type PillboxInstallationRef = {
  readonly installation_id: string;
  readonly execution_realm_id: string;
  readonly protocol_revision: "pillbox.huddles/1";
};

/**
 * Signed-envelope verification evidence sent to Huddles currentness.
 *
 * The fingerprint is over the decoded 32-byte Ed25519 public key, not its
 * textual `ed25519:` representation. Huddles uses both fields to detect a
 * retired key or a Pillbox verifier configured with a different key material.
 */
export type PillboxVerifiedSigner = {
  readonly algorithm: "Ed25519";
  readonly key_id: string;
  readonly public_key_sha256: `sha256:${string}`;
};

/** Generic execution/2 authorization is singular and operation-scoped. */
export type PillboxExecutionOperation = "execute" | "status" | "cancel";

export type PillboxExecutionOperationGrantClaims = {
  readonly version: "huddles.execution-operation-grant/2";
  readonly grant_id: string;
  readonly installation: PillboxInstallationRef;
  readonly organization_id: string;
  readonly workspace_id: string;
  readonly principal_id: string;
  readonly policy_id: string;
  readonly operation: PillboxExecutionOperation;
  readonly invocation_id: string;
  readonly request_digest: `sha256:${string}`;
  readonly issued_at: number;
  readonly not_before: number;
  readonly expires_at: number;
};

export type SignedPillboxExecutionOperationGrant = {
  readonly algorithm: "Ed25519";
  readonly key_id: string;
  readonly claims: PillboxExecutionOperationGrantClaims;
  readonly signature: string;
};

export type PillboxExecutionOperationGrantIssueRequest = {
  readonly grant_id: string;
  readonly installation_id: string;
  readonly workspace_id: string;
  readonly principal_id: string;
  readonly policy_id: string;
  readonly operation: PillboxExecutionOperation;
  readonly invocation_id: string;
  readonly request_digest: `sha256:${string}`;
  readonly ttl_seconds: number;
};

/** The signed sidecar passed separately from an identity-pure execution/2 request. */
export type PillboxExecutionOperationGrantIssueResponse = {
  readonly grant: SignedPillboxExecutionOperationGrant;
};

export type PillboxExecutionOperationBinding = {
  readonly operation: PillboxExecutionOperation;
  readonly invocation_id: string;
  readonly request_digest: `sha256:${string}`;
};

export const PILLBOX_EXECUTION_OPERATION_CURRENTNESS_VERSION =
  "pillbox.authorization-currentness/3" as const;

export type PillboxExecutionOperationGrantUseRequest = {
  readonly version: typeof PILLBOX_EXECUTION_OPERATION_CURRENTNESS_VERSION;
  readonly grant: SignedPillboxExecutionOperationGrant;
  readonly expected: PillboxExecutionOperationBinding;
  readonly verified_signer: PillboxVerifiedSigner;
};

export function makeExecutionOperationGrantCurrentnessRequest(
  grant: SignedPillboxExecutionOperationGrant,
  expected: PillboxExecutionOperationBinding,
  verifiedSigner: PillboxVerifiedSigner,
): PillboxExecutionOperationGrantUseRequest {
  return {
    version: PILLBOX_EXECUTION_OPERATION_CURRENTNESS_VERSION,
    grant,
    expected,
    verified_signer: verifiedSigner,
  };
}

export class ManagedContractError extends Error {
  readonly code = "invalid_managed_contract" as const;
}

const nonEmpty = (value: unknown, field: string): string => {
  if (typeof value !== "string" || value.length === 0) {
    throw new ManagedContractError(`${field} must be a non-empty string`);
  }
  return value;
};

const digest = (value: unknown, field: string): `sha256:${string}` => {
  const text = nonEmpty(value, field);
  if (!/^sha256:[0-9a-f]{64}$/.test(text)) {
    throw new ManagedContractError(`${field} must be a lowercase sha256 digest`);
  }
  return text as `sha256:${string}`;
};

function installation(value: unknown): PillboxInstallationRef {
  if (!record(value)) throw new ManagedContractError("installation must be an object");
  rejectUnknown(value, ["installation_id", "execution_realm_id", "protocol_revision"], "installation");
  const ref = {
    installation_id: nonEmpty(value.installation_id, "installation.installation_id"),
    execution_realm_id: nonEmpty(value.execution_realm_id, "installation.execution_realm_id"),
    protocol_revision: value.protocol_revision,
  } as PillboxInstallationRef;
  if (ref.protocol_revision !== "pillbox.huddles/1") {
    throw new ManagedContractError("unsupported Pillbox protocol revision");
  }
  return ref;
}

export function validateExecutionOperationGrantClaims(
  value: unknown,
): PillboxExecutionOperationGrantClaims {
  if (!record(value)) {
    throw new ManagedContractError("execution operation grant claims must be an object");
  }
  rejectUnknown(
    value,
    [
      "version",
      "grant_id",
      "installation",
      "organization_id",
      "workspace_id",
      "principal_id",
      "policy_id",
      "operation",
      "invocation_id",
      "request_digest",
      "issued_at",
      "not_before",
      "expires_at",
    ],
    "execution operation grant claims",
  );
  if (value.version !== "huddles.execution-operation-grant/2") {
    throw new ManagedContractError("unsupported execution operation grant version");
  }
  const operation = executionOperation(value.operation, "operation");
  const issuedAt = integer(value.issued_at, "issued_at");
  const notBefore = integer(value.not_before, "not_before");
  const expiresAt = integer(value.expires_at, "expires_at");
  if (!(issuedAt <= notBefore && notBefore < expiresAt)) {
    throw new ManagedContractError(
      "execution operation grant timestamps must satisfy issued_at <= not_before < expires_at",
    );
  }
  return {
    version: "huddles.execution-operation-grant/2",
    grant_id: nonEmpty(value.grant_id, "grant_id"),
    installation: installation(value.installation),
    organization_id: nonEmpty(value.organization_id, "organization_id"),
    workspace_id: nonEmpty(value.workspace_id, "workspace_id"),
    principal_id: nonEmpty(value.principal_id, "principal_id"),
    policy_id: nonEmpty(value.policy_id, "policy_id"),
    operation,
    invocation_id: nonEmpty(value.invocation_id, "invocation_id"),
    request_digest: digest(value.request_digest, "request_digest"),
    issued_at: issuedAt,
    not_before: notBefore,
    expires_at: expiresAt,
  };
}

export function validateSignedExecutionOperationGrant(
  value: unknown,
): SignedPillboxExecutionOperationGrant {
  if (!record(value) || value.algorithm !== "Ed25519") {
    throw new ManagedContractError("signed execution operation grant algorithm is invalid");
  }
  rejectUnknown(
    value,
    ["algorithm", "key_id", "claims", "signature"],
    "signed execution operation grant",
  );
  const signature = nonEmpty(value.signature, "grant.signature");
  if (!/^[A-Za-z0-9_-]+$/.test(signature)) {
    throw new ManagedContractError("grant signature must be unpadded base64url");
  }
  return {
    algorithm: "Ed25519",
    key_id: nonEmpty(value.key_id, "grant.key_id"),
    claims: validateExecutionOperationGrantClaims(value.claims),
    signature,
  };
}

export function validateExecutionOperationGrantIssueRequest(
  value: unknown,
): PillboxExecutionOperationGrantIssueRequest {
  if (!record(value)) {
    throw new ManagedContractError("execution operation grant issue request must be an object");
  }
  rejectUnknown(
    value,
    [
      "grant_id",
      "installation_id",
      "workspace_id",
      "principal_id",
      "policy_id",
      "operation",
      "invocation_id",
      "request_digest",
      "ttl_seconds",
    ],
    "execution operation grant issue request",
  );
  const ttlSeconds = integer(value.ttl_seconds, "ttl_seconds");
  if (ttlSeconds <= 0 || ttlSeconds > 300) {
    throw new ManagedContractError("ttl_seconds must be a positive integer at most 300");
  }
  return {
    grant_id: nonEmpty(value.grant_id, "grant_id"),
    installation_id: nonEmpty(value.installation_id, "installation_id"),
    workspace_id: nonEmpty(value.workspace_id, "workspace_id"),
    principal_id: nonEmpty(value.principal_id, "principal_id"),
    policy_id: nonEmpty(value.policy_id, "policy_id"),
    operation: executionOperation(value.operation, "operation"),
    invocation_id: nonEmpty(value.invocation_id, "invocation_id"),
    request_digest: digest(value.request_digest, "request_digest"),
    ttl_seconds: ttlSeconds,
  };
}

export function validateExecutionOperationGrantIssueResponse(
  value: unknown,
): PillboxExecutionOperationGrantIssueResponse {
  if (!record(value)) {
    throw new ManagedContractError("execution operation grant issue response must be an object");
  }
  rejectUnknown(value, ["grant"], "execution operation grant issue response");
  return { grant: validateSignedExecutionOperationGrant(value.grant) };
}

export function validateExecutionOperationBinding(
  claims: PillboxExecutionOperationGrantClaims,
  expected: PillboxExecutionOperationBinding,
): "operation_mismatch" | "invocation_mismatch" | "request_digest_mismatch" | undefined {
  if (claims.operation !== expected.operation) return "operation_mismatch";
  if (claims.invocation_id !== expected.invocation_id) return "invocation_mismatch";
  if (claims.request_digest !== expected.request_digest) return "request_digest_mismatch";
  return undefined;
}

export function isExecutionOperationGrantCurrent(
  claims: PillboxExecutionOperationGrantClaims,
  now: number,
): boolean {
  return claims.not_before <= now && now < claims.expires_at;
}

function integer(value: unknown, field: string): number { if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new ManagedContractError(`${field} must be a non-negative integer`); return value; }
function record(value: unknown): value is Record<string, unknown> { return typeof value === "object" && value !== null && !Array.isArray(value); }

export function managedCanonicalJson(value: unknown): string {
  if (value === undefined || typeof value === "function" || typeof value === "symbol") throw new ManagedContractError("managed hash material must be JSON-compatible");
  if (value === null || typeof value !== "object") { if (typeof value === "number" && !Number.isFinite(value)) throw new ManagedContractError("managed hash material contains a non-finite number"); return JSON.stringify(value); }
  if (Array.isArray(value)) return `[${value.map(managedCanonicalJson).join(",")}]`;
  const object = value as Record<string, unknown>;
  return `{${Object.entries(object)
    .sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0)
    .map(([key, member]) => `${JSON.stringify(key)}:${managedCanonicalJson(member)}`)
    .join(",")}}`;
}

function rejectUnknown(value: Record<string, unknown>, allowed: readonly string[], path: string): void {
  const permitted = new Set(allowed);
  const unknown = Object.keys(value).find((key) => !permitted.has(key));
  if (unknown) throw new ManagedContractError(`${path} contains unrecognized field '${unknown}'`);
}

function executionOperation(value: unknown, field: string): PillboxExecutionOperation {
  if (value !== "execute" && value !== "status" && value !== "cancel") {
    throw new ManagedContractError(`${field} must be execute, status, or cancel`);
  }
  return value;
}
