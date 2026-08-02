import {
  isManagedGrantCurrent,
  validateEvidenceBinding,
  validateEvidenceReadClaims,
  validatePositionalEvidenceSelector,
  validateSignedExecutionGrant,
  type PillboxEvidenceReadBinding,
  type PillboxEvidenceReadGrantClaims,
  type PillboxSessionRef,
  type SignedPillboxEvidenceReadGrant,
} from "./managed_contract.js";
import { verifyManagedEd25519Signature } from "./managed_signature.js";

export type EvidenceFrame = { readonly seq: number; readonly sessionId: string; readonly at: string; readonly actor?: unknown; readonly payload: unknown };

export type EvidenceReadRequest = {
  readonly grant: SignedPillboxEvidenceReadGrant;
  readonly session_ref: PillboxSessionRef;
  readonly max_events?: number;
};

export class EvidenceReadError extends Error {
  readonly code = "evidence_unavailable" as const;
}

/** Viewer-bound evidence validation; no HCP or WorkEvent mutation happens here. */
export async function authorizeEvidenceRead(input: {
  readonly request: EvidenceReadRequest;
  readonly keyId?: string;
  readonly publicKey?: string;
  readonly now?: number;
}): Promise<PillboxEvidenceReadGrantClaims> {
  let envelope;
  try { envelope = validateEvidenceEnvelope(input.request.grant); } catch (cause) { throw new EvidenceReadError(`evidence grant is invalid: ${safe(cause)}`); }
  if (envelope.key_id !== input.keyId || !input.publicKey) throw new EvidenceReadError("evidence grant key is not trusted");
  try {
    await verifyManagedEd25519Signature({ publicKeyMaterial: input.publicKey, signature: envelope.signature, claims: envelope.claims });
  } catch (cause) { throw new EvidenceReadError(`evidence grant signature is invalid: ${safe(cause)}`); }
  const claims = validateEvidenceReadClaims(envelope.claims);
  if (!isManagedGrantCurrent(claims, input.now ?? Math.floor(Date.now() / 1000))) throw new EvidenceReadError("evidence grant is expired");
  let requested: PillboxSessionRef;
  try { requested = validateSupportedEvidenceSelector(input.request.session_ref); } catch (cause) { throw new EvidenceReadError(`evidence session reference is invalid: ${safe(cause)}`); }
  if (claims.session_ref.seq === undefined && claims.session_ref.seq_range === undefined) throw new EvidenceReadError("evidence grant has no supported positional selector");
  const expected: PillboxEvidenceReadBinding = { installation: claims.installation, workspace_id: claims.workspace_id, viewer_principal_id: claims.viewer_principal_id, policy_id: claims.policy_id, run_id: claims.run_id, session_ref: requested };
  const mismatch = validateEvidenceBinding(claims, expected);
  if (mismatch) throw new EvidenceReadError(`evidence grant ${mismatch}`);
  return claims;
}

export function validateSupportedEvidenceSelector(value: unknown): PillboxSessionRef {
  try {
    return validatePositionalEvidenceSelector(value);
  } catch (cause) {
    throw new EvidenceReadError(cause instanceof Error ? cause.message : "evidence session selector is invalid");
  }
}

function validateEvidenceEnvelope(value: unknown): SignedPillboxEvidenceReadGrant {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new EvidenceReadError("evidence envelope is invalid");
  const raw = value as Record<string, unknown>;
  if (raw.algorithm !== "Ed25519" || typeof raw.key_id !== "string" || typeof raw.signature !== "string") throw new EvidenceReadError("evidence envelope is invalid");
  if (Object.keys(raw).some((key) => !["algorithm", "key_id", "claims", "signature"].includes(key))) throw new EvidenceReadError("evidence envelope contains an unrecognized field");
  return { algorithm: "Ed25519", key_id: raw.key_id, signature: raw.signature, claims: validateEvidenceReadClaims(raw.claims) };
}

function safe(cause: unknown): string { return cause instanceof Error ? cause.message.slice(0, 120) : "invalid evidence grant"; }
