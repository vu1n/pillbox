import type { PillboxExecutionGrantBinding, SignedPillboxExecutionGrant } from "../managed_contract.js";

export type CredentialBinding = {
  readonly credential_binding_id: string;
  readonly secret_ref: string;
  readonly purpose: string;
};

export type OAuthMaterial = {
  readonly access_token: string;
  readonly refresh_token?: string;
  readonly expires_at: number;
  readonly provider_host: string;
  readonly generation: number;
};

export type AuthorizedCredentialRoute = CredentialBinding & {
  readonly host: string;
};

export type CredentialLeaseRequest = {
  readonly route: AuthorizedCredentialRoute;
  readonly invocation_id: string;
  readonly execution_realm_id: string;
};

export type ManagedOutboundParams = CredentialLeaseRequest & {
  readonly grant: SignedPillboxExecutionGrant;
  readonly expected: PillboxExecutionGrantBinding;
};

export type CredentialLease = {
  readonly lease_id: string;
  readonly generation: number;
  readonly provider_host: string;
  readonly expires_at: number;
  /** Deliberately returned only across the trusted Worker-side handler seam. */
  readonly access_token: string;
};

export type CredentialRefreshResult =
  | { readonly status: "stored"; readonly generation: number; readonly expires_at: number }
  | { readonly status: "reauth_required"; readonly reason: "ambiguous_refresh" | "provider_rejected" };

export function isCurrentRefreshGeneration(input: { readonly generation: number; readonly pending_generation: number | null; readonly status: string }, expectedGeneration: number): boolean {
  return input.status === "active" && input.generation === expectedGeneration && input.pending_generation === expectedGeneration + 1;
}

export class CredentialBrokerError extends Error {
  readonly code:
    | "invalid_request"
    | "not_found"
    | "revoked"
    | "wrong_host"
    | "reauth_required"
    | "encryption_unavailable"
    | "refresh_in_progress";

  constructor(code: CredentialBrokerError["code"], message: string, cause?: unknown) {
    super(message, { cause });
    this.name = "CredentialBrokerError";
    this.code = code;
  }
}
