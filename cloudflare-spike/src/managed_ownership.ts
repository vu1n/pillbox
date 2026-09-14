import type { ManagedCapability } from "./auth.js";
import type { PillboxExecutionOperationGrantClaims } from "./managed_contract.js";
import { managedCanonicalJson } from "./managed_contract.js";
import { sha256Hex } from "./runtime_identity.js";

export type ManagedExecutionOwnerDomain =
  | "huddles_workspace"
  | "public_controller";

export type ManagedExecutionOwner = {
  readonly domain: ManagedExecutionOwnerDomain;
  readonly digest: `sha256:${string}`;
};

export async function huddlesExecutionOwner(
  claims: PillboxExecutionOperationGrantClaims,
): Promise<ManagedExecutionOwner> {
  return ownerDigest("huddles_workspace", {
    installation_id: claims.installation.installation_id,
    execution_realm_id: claims.installation.execution_realm_id,
    organization_id: claims.organization_id,
    workspace_id: claims.workspace_id,
  });
}

export async function publicControllerOwner(
  capability: Pick<ManagedCapability, "subject">,
): Promise<ManagedExecutionOwner> {
  return ownerDigest("public_controller", { subject: capability.subject });
}

async function ownerDigest(
  domain: ManagedExecutionOwnerDomain,
  identity: Readonly<Record<string, string>>,
): Promise<ManagedExecutionOwner> {
  const digest = await sha256Hex(
    managedCanonicalJson({
      version: "pillbox.execution-owner/1",
      domain,
      ...identity,
    }),
  );
  return { domain, digest: `sha256:${digest}` };
}
