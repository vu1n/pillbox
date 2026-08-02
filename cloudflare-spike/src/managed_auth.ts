import type { Env } from "./worker.js";
import { verifyManagedEd25519Signature } from "./managed_signature.js";
import {
  managedCanonicalJson,
  isManagedGrantCurrent,
  type PillboxExecutionGrantBinding,
  type PillboxExecutionGrantClaims,
  type SignedPillboxExecutionGrant,
  validateExecutionGrantClaims,
  validateGrantBinding,
  validateSignedExecutionGrant,
} from "./managed_contract.js";

export interface PillboxAuthorizationControlPlane {
  authorizeExecutionGrant(input: {
    grant: PillboxExecutionGrantClaims;
    expected: PillboxExecutionGrantBinding;
  }): Promise<PillboxExecutionGrantClaims>;
  authorizeEvidenceReadGrant?(input: unknown): Promise<unknown>;
}

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
    await verifyManagedEd25519Signature({ publicKeyMaterial, signature: envelope.signature, claims: envelope.claims });
    return validateExecutionGrantClaims(envelope.claims);
  } catch (cause) {
    throw new ManagedAuthorizationError("invalid_grant", "managed execution grant signature is invalid", cause);
  }
}

/** Verify, bind, and re-introspect a grant at every sensitive operation. */
export async function authorizeExecutionGrant(
  env: Env,
  value: unknown,
  expected: PillboxExecutionGrantBinding,
): Promise<PillboxExecutionGrantClaims> {
  const claims = await verifySignedExecutionGrant(
    value,
    env.PILLBOX_GRANT_KEY_ID,
    env.PILLBOX_GRANT_PUBLIC_KEY,
  );
  const pins: readonly [string, string | undefined, string][] = [
    ["installation", env.PILLBOX_INSTALLATION_ID, claims.installation.installation_id],
    ["execution realm", env.PILLBOX_EXECUTION_REALM_ID, claims.installation.execution_realm_id],
    ["protocol", env.PILLBOX_PROTOCOL_REVISION, claims.installation.protocol_revision],
  ];
  for (const [label, configured, actual] of pins) {
    if (!configured) throw new ManagedAuthorizationError("authorization_unavailable", `managed ${label} pin is not configured`);
    if (configured !== actual) throw new ManagedAuthorizationError("grant_binding_mismatch", `managed ${label} does not match this deployment`);
  }
  if (!env.PILLBOX_ORGANIZATION_ID) throw new ManagedAuthorizationError("authorization_unavailable", "managed organization pin is not configured");
  if (env.PILLBOX_ORGANIZATION_ID !== claims.organization_id) throw new ManagedAuthorizationError("grant_binding_mismatch", "managed organization does not match this deployment");
  const now = Math.floor(Date.now() / 1000);
  if (!isManagedGrantCurrent(claims, now)) {
    throw new ManagedAuthorizationError("grant_expired", "managed execution grant is outside its validity interval");
  }
  const mismatch = validateGrantBinding(claims, expected);
  if (mismatch) throw new ManagedAuthorizationError("grant_binding_mismatch", `managed grant ${mismatch}`);
  const controlPlane = env.PillboxAuthorizationControlPlane;
  if (!controlPlane) throw new ManagedAuthorizationError("authorization_unavailable", "Pillbox authorization control plane is not configured");
  try {
    const current = await controlPlane.authorizeExecutionGrant({ grant: claims, expected });
    const validated = validateExecutionGrantClaims(current);
    if (managedCanonicalJson(validated) !== managedCanonicalJson(claims)) {
      throw new ManagedAuthorizationError("grant_revoked", "authorization control plane returned different grant claims");
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
