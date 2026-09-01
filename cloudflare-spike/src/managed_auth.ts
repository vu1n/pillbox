import type { Env } from "./worker.js";
import type {
  CancelInvocationV2Request,
  ExecuteInvocationV2Request,
  GetInvocationV2Request,
} from "./codex_execution.js";
import { verifyManagedEd25519Signature } from "./managed_signature.js";
import {
  decodeManagedEd25519PublicKey,
  makeManagedVerifiedSigner,
} from "./managed_signer.js";
import {
  isExecutionOperationGrantCurrent,
  managedCanonicalJson,
  makeExecutionOperationGrantCurrentnessRequest,
  type PillboxExecutionOperation,
  type PillboxExecutionOperationBinding,
  type PillboxExecutionOperationGrantClaims,
  type PillboxExecutionOperationGrantIssueResponse,
  type PillboxExecutionOperationGrantUseRequest,
  type PillboxVerifiedSigner,
  type SignedPillboxExecutionOperationGrant,
  validateExecutionOperationBinding,
  validateExecutionOperationGrantClaims,
  validateExecutionOperationGrantIssueResponse,
} from "./managed_contract.js";
import { sha256Hex } from "./runtime_identity.js";

export interface PillboxAuthorizationCurrentness {
  authorizeExecutionOperationGrant(
    input: PillboxExecutionOperationGrantUseRequest,
  ): Promise<PillboxExecutionOperationGrantClaims>;
}

export type ExecutionOperationRequest =
  | ExecuteInvocationV2Request
  | GetInvocationV2Request
  | CancelInvocationV2Request;

export class ManagedAuthorizationError extends Error {
  readonly code:
    | "authorization_unavailable"
    | "invalid_grant"
    | "grant_expired"
    | "grant_binding_mismatch"
    | "grant_revoked";

  constructor(code: ManagedAuthorizationError["code"], message: string, cause?: unknown) {
    super(message, { cause });
    this.name = "ManagedAuthorizationError";
    this.code = code;
  }
}

export class ManagedBootstrapError extends Error {
  readonly code = "managed_bootstrap_unavailable" as const;

  constructor(message: string) {
    super(message);
    this.name = "ManagedBootstrapError";
  }
}

export interface ManagedBurninBootstrapEnvironment {
  readonly PILLBOX_BOOTSTRAP_REQUIRED?: string;
  readonly MANAGED_CAPABILITY_SECRET?: string;
  readonly PILLBOX_GRANT_KEY_ID?: string;
  readonly PILLBOX_GRANT_PUBLIC_KEY?: string;
  readonly PILLBOX_INSTALLATION_ID?: string;
  readonly PILLBOX_EXECUTION_REALM_ID?: string;
  readonly PILLBOX_PROTOCOL_REVISION?: string;
  readonly PILLBOX_ORGANIZATION_ID?: string;
  readonly PILLBOX_MANAGED_CONCURRENCY?: string;
  readonly MANAGED_EXECUTION_EPOCH?: string;
  readonly MANAGED_EXECUTION_LIMIT?: string;
  readonly PillboxAuthorizationCurrentness?: PillboxAuthorizationCurrentness;
}

/**
 * Runtime half of the isolated burn-in bootstrap gate. Static service names,
 * resource IDs, key fingerprints, and container concurrency are checked by
 * scripts/validate-burnin-bootstrap.mjs before deployment.
 */
export function requireManagedBurninBootstrap(
  env: ManagedBurninBootstrapEnvironment,
): void {
  if (env.PILLBOX_BOOTSTRAP_REQUIRED === undefined) return;
  if (env.PILLBOX_BOOTSTRAP_REQUIRED !== "1") {
    throw new ManagedBootstrapError("managed burn-in bootstrap marker is not configured");
  }

  const pins: readonly [string, string | undefined][] = [
    ["grant key ID", env.PILLBOX_GRANT_KEY_ID],
    ["grant public key", env.PILLBOX_GRANT_PUBLIC_KEY],
    ["installation", env.PILLBOX_INSTALLATION_ID],
    ["execution realm", env.PILLBOX_EXECUTION_REALM_ID],
    ["organization", env.PILLBOX_ORGANIZATION_ID],
    ["execution epoch", env.MANAGED_EXECUTION_EPOCH],
  ];
  for (const [label, value] of pins) {
    if (!isConfiguredBootstrapValue(value)) {
      throw new ManagedBootstrapError(`managed burn-in ${label} is not configured`);
    }
  }
  if (!isManagedEd25519PublicKey(env.PILLBOX_GRANT_PUBLIC_KEY!)) {
    throw new ManagedBootstrapError("managed burn-in grant public key is invalid");
  }
  if (env.PILLBOX_PROTOCOL_REVISION !== "pillbox.huddles/1") {
    throw new ManagedBootstrapError("managed burn-in protocol pin is not configured");
  }
  if (
    env.PILLBOX_MANAGED_CONCURRENCY !== "1" ||
    env.MANAGED_EXECUTION_LIMIT !== env.PILLBOX_MANAGED_CONCURRENCY
  ) {
    throw new ManagedBootstrapError("managed burn-in concurrency pins do not match");
  }
  if (
    typeof env.PillboxAuthorizationCurrentness?.authorizeExecutionOperationGrant !==
    "function"
  ) {
    throw new ManagedBootstrapError(
      "managed burn-in authorization currentness service is not configured",
    );
  }
}

/** Public HTTP adds an HMAC capability without weakening private grant authorization. */
export function requireManagedBurninHttpBootstrap(
  env: ManagedBurninBootstrapEnvironment,
): void {
  requireManagedBurninBootstrap(env);
  if (env.PILLBOX_BOOTSTRAP_REQUIRED === undefined) return;
  if (!isConfiguredBootstrapValue(env.MANAGED_CAPABILITY_SECRET)) {
    throw new ManagedBootstrapError(
      "managed burn-in capability secret is not configured",
    );
  }
  if (env.MANAGED_CAPABILITY_SECRET.length < 32) {
    throw new ManagedBootstrapError("managed burn-in capability secret is invalid");
  }
}

function isConfiguredBootstrapValue(value: string | undefined): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    !/placeholder|replace|unconfigured/i.test(value) &&
    !/^<.*>$/.test(value)
  );
}

function isManagedEd25519PublicKey(value: string): boolean {
  try {
    return new Set(decodeManagedEd25519PublicKey(value)).size > 1;
  } catch {
    return false;
  }
}

export async function verifySignedExecutionOperationGrantWithSigner(
  value: unknown,
  keyId: string | undefined,
  publicKeyMaterial: string | undefined,
): Promise<{
  readonly grant: SignedPillboxExecutionOperationGrant;
  readonly claims: PillboxExecutionOperationGrantClaims;
  readonly verified_signer: PillboxVerifiedSigner;
}> {
  let authorization: PillboxExecutionOperationGrantIssueResponse;
  try {
    authorization = validateExecutionOperationGrantIssueResponse(value);
  } catch (cause) {
    throw new ManagedAuthorizationError(
      "invalid_grant",
      "managed execution operation authorization is invalid",
      cause,
    );
  }
  const grant = authorization.grant;
  if (!keyId || grant.key_id !== keyId || !publicKeyMaterial) {
    throw new ManagedAuthorizationError(
      "invalid_grant",
      "managed execution operation grant key is not trusted",
    );
  }
  try {
    const verified = await verifyManagedEd25519Signature({
      publicKeyMaterial,
      signature: grant.signature,
      claims: grant.claims,
    });
    return {
      grant,
      claims: grant.claims,
      verified_signer: makeManagedVerifiedSigner(
        grant.key_id,
        verified.public_key_sha256,
      ),
    };
  } catch (cause) {
    throw new ManagedAuthorizationError(
      "invalid_grant",
      "managed execution operation grant signature is invalid",
      cause,
    );
  }
}

/** Fail-closed authorization for one identity-pure execution/2 operation. */
export async function authorizeExecutionOperation(
  env: Env,
  authorization: unknown,
  operation: PillboxExecutionOperation,
  request: ExecutionOperationRequest,
): Promise<void> {
  const verified = await verifySignedExecutionOperationGrantWithSigner(
    authorization,
    env.PILLBOX_GRANT_KEY_ID,
    env.PILLBOX_GRANT_PUBLIC_KEY,
  );
  const claims = verified.claims;
  validateDeploymentPins(env, claims);
  const now = Math.floor(Date.now() / 1_000);
  if (!isExecutionOperationGrantCurrent(claims, now)) {
    throw new ManagedAuthorizationError(
      "grant_expired",
      "managed execution operation grant is outside its validity interval",
    );
  }
  const expected: PillboxExecutionOperationBinding = {
    operation,
    invocation_id: request.invocation_id,
    request_digest: await executionOperationRequestDigest(request),
  };
  const mismatch = validateExecutionOperationBinding(claims, expected);
  if (mismatch) {
    throw new ManagedAuthorizationError(
      "grant_binding_mismatch",
      `managed execution operation grant ${mismatch}`,
    );
  }
  const currentness = env.PillboxAuthorizationCurrentness;
  if (!currentness) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      "Pillbox authorization currentness service is not configured",
    );
  }
  try {
    const current = await currentness.authorizeExecutionOperationGrant(
      makeExecutionOperationGrantCurrentnessRequest(
        verified.grant,
        expected,
        verified.verified_signer,
      ),
    );
    const validated = validateExecutionOperationGrantClaims(current);
    if (managedCanonicalJson(validated) !== managedCanonicalJson(claims)) {
      throw new ManagedAuthorizationError(
        "grant_revoked",
        "authorization currentness service returned different operation grant claims",
      );
    }
    if (!isExecutionOperationGrantCurrent(validated, Math.floor(Date.now() / 1_000))) {
      throw new ManagedAuthorizationError(
        "grant_expired",
        "managed execution operation grant expired during authorization",
      );
    }
  } catch (cause) {
    if (cause instanceof ManagedAuthorizationError) throw cause;
    throw new ManagedAuthorizationError(
      "grant_revoked",
      "managed execution operation grant is not current",
      cause,
    );
  }
}

export async function executionOperationRequestDigest(
  request: ExecutionOperationRequest,
): Promise<`sha256:${string}`> {
  return `sha256:${await sha256Hex(managedCanonicalJson(request))}`;
}

function validateDeploymentPins(
  env: Env,
  claims: {
    readonly installation: {
      readonly installation_id: string;
      readonly execution_realm_id: string;
      readonly protocol_revision: string;
    };
    readonly organization_id: string;
  },
): void {
  const pins: readonly [string, string | undefined, string][] = [
    ["installation", env.PILLBOX_INSTALLATION_ID, claims.installation.installation_id],
    [
      "execution realm",
      env.PILLBOX_EXECUTION_REALM_ID,
      claims.installation.execution_realm_id,
    ],
    ["protocol", env.PILLBOX_PROTOCOL_REVISION, claims.installation.protocol_revision],
  ];
  for (const [label, configured, actual] of pins) {
    if (!configured) {
      throw new ManagedAuthorizationError(
        "authorization_unavailable",
        `managed ${label} pin is not configured`,
      );
    }
    if (configured !== actual) {
      throw new ManagedAuthorizationError(
        "grant_binding_mismatch",
        `managed ${label} does not match this deployment`,
      );
    }
  }
  if (!env.PILLBOX_ORGANIZATION_ID) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      "managed organization pin is not configured",
    );
  }
  if (env.PILLBOX_ORGANIZATION_ID !== claims.organization_id) {
    throw new ManagedAuthorizationError(
      "grant_binding_mismatch",
      "managed organization does not match this deployment",
    );
  }
}
