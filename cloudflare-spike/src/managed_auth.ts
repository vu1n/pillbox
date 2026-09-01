import type { Env } from "./worker.js";
import type {
  CancelInvocationV2Request,
  ExecuteInvocationV2Request,
  GetInvocationV2Request,
} from "./codex_execution.js";
import { verifyManagedEd25519Signature } from "./managed_signature.js";
import { makeManagedVerifiedSigner } from "./managed_signer.js";
import {
  isExecutionOperationGrantCurrent,
  managedCanonicalJson,
  isManagedGrantCurrent,
  makeExecutionOperationGrantCurrentnessRequest,
  makeExecutionGrantCurrentnessRequest,
  type PillboxExecutionOperation,
  type PillboxExecutionOperationBinding,
  type PillboxExecutionOperationGrantClaims,
  type PillboxExecutionOperationGrantIssueResponse,
  type PillboxExecutionOperationGrantUseRequest,
  type PillboxExecutionGrantCurrentnessRequest,
  type PillboxExecutionGrantBinding,
  type PillboxExecutionGrantClaims,
  type PillboxVerifiedSigner,
  type SignedPillboxExecutionOperationGrant,
  type SignedPillboxExecutionGrant,
  validateExecutionOperationBinding,
  validateExecutionOperationGrantClaims,
  validateExecutionOperationGrantIssueResponse,
  validateExecutionGrantClaims,
  validateGrantBinding,
  validateSignedExecutionGrant,
} from "./managed_contract.js";
import { sha256Hex } from "./runtime_identity.js";

export interface PillboxAuthorizationCurrentness {
  /** Currentness v2 is deliberately the same method name: old exact v1 request schemas must reject rather than downgrade. */
  authorizeExecutionGrant(input: PillboxExecutionGrantCurrentnessRequest): Promise<PillboxExecutionGrantClaims>;
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

/** Verify the Huddles envelope before the service binding is contacted. */
export async function verifySignedExecutionGrant(
  value: unknown,
  keyId: string | undefined,
  publicKeyMaterial: string | undefined,
): Promise<PillboxExecutionGrantClaims> {
  return (await verifySignedExecutionGrantWithSigner(value, keyId, publicKeyMaterial)).claims;
}

export async function verifySignedExecutionGrantWithSigner(
  value: unknown,
  keyId: string | undefined,
  publicKeyMaterial: string | undefined,
): Promise<{ readonly claims: PillboxExecutionGrantClaims; readonly verified_signer: PillboxVerifiedSigner }> {
  let envelope: SignedPillboxExecutionGrant;
  try {
    envelope = validateSignedExecutionGrant(value);
  } catch (cause) {
    throw new ManagedAuthorizationError("invalid_grant", "managed execution grant is invalid", cause);
  }
  if (!keyId || envelope.key_id !== keyId || !publicKeyMaterial) {
    throw new ManagedAuthorizationError("invalid_grant", "managed execution grant key is not trusted");
  }
  try {
    const verified = await verifyManagedEd25519Signature({ publicKeyMaterial, signature: envelope.signature, claims: envelope.claims });
    return {
      claims: validateExecutionGrantClaims(envelope.claims),
      verified_signer: makeManagedVerifiedSigner(envelope.key_id, verified.public_key_sha256),
    };
  } catch (cause) {
    throw new ManagedAuthorizationError("invalid_grant", "managed execution grant signature is invalid", cause);
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
      claims: validateExecutionOperationGrantClaims(grant.claims),
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
      "Pillbox authorization control plane is not configured",
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

/** Verify, bind, and re-introspect a grant at every sensitive operation. */
export async function authorizeExecutionGrant(
  env: Env,
  value: unknown,
  expected: PillboxExecutionGrantBinding,
): Promise<PillboxExecutionGrantClaims> {
  const verified = await verifySignedExecutionGrantWithSigner(
    value,
    env.PILLBOX_GRANT_KEY_ID,
    env.PILLBOX_GRANT_PUBLIC_KEY,
  );
  const claims = verified.claims;
  validateDeploymentPins(env, claims);
  const now = Math.floor(Date.now() / 1000);
  if (!isManagedGrantCurrent(claims, now)) {
    throw new ManagedAuthorizationError("grant_expired", "managed execution grant is outside its validity interval");
  }
  const mismatch = validateGrantBinding(claims, expected);
  if (mismatch) throw new ManagedAuthorizationError("grant_binding_mismatch", `managed grant ${mismatch}`);
  const currentness = env.PillboxAuthorizationCurrentness;
  if (!currentness) throw new ManagedAuthorizationError("authorization_unavailable", "Pillbox authorization currentness service is not configured");
  try {
    const current = await currentness.authorizeExecutionGrant(makeExecutionGrantCurrentnessRequest(claims, expected, verified.verified_signer));
    const validated = validateExecutionGrantClaims(current);
    if (managedCanonicalJson(validated) !== managedCanonicalJson(claims)) {
      throw new ManagedAuthorizationError("grant_revoked", "authorization currentness service returned different grant claims");
    }
    if (!isManagedGrantCurrent(validated, Math.floor(Date.now() / 1000))) {
      throw new ManagedAuthorizationError("grant_expired", "managed execution grant expired during authorization");
    }
    return validated;
  } catch (cause) {
    if (cause instanceof ManagedAuthorizationError) throw cause;
    throw new ManagedAuthorizationError("grant_revoked", "managed execution grant is not current", cause);
  }
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
