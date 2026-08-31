import type { Env } from "./worker.js";
import type {
  EnsureSessionRequest,
  InvokeSessionRequest,
} from "./legacy_huddles_adapter.js";
import {
  authorizeExecutionGrant,
  ManagedAuthorizationError,
} from "./managed_auth.js";
import {
  managedCanonicalJson,
  type PillboxExecutionGrantBinding,
  type PillboxInstallationRef,
  type PillboxManagedRequestBinding,
} from "./managed_contract.js";
import { sha256Hex } from "./runtime_identity.js";

/** Authorize the stateless Huddles compatibility projection before it returns. */
export async function authorizeManagedEnsure(
  env: Env,
  request: EnsureSessionRequest,
): Promise<string | undefined> {
  const authorization = request.managed_authorization;
  if (!authorization) {
    if (managedAuthRequired(env, request.canonical_request.execution !== undefined)) {
      throw new ManagedAuthorizationError(
        "invalid_grant",
        "managed execution grant is required",
      );
    }
    return undefined;
  }
  rejectCredentialMaterial(request.canonical_request, "canonical_request");
  const binding = authorization.request_binding;
  if (
    binding.session_idempotency_key !== request.effect_id ||
    binding.run_id !== required(request.canonical_request.run_id, "run_id") ||
    binding.packet_id !== required(request.canonical_request.packet_id, "packet_id") ||
    binding.principal_id !==
      required(request.canonical_request.activity_principal_id, "activity_principal_id") ||
    binding.policy_id !== required(request.canonical_request.policy_id, "policy_id")
  ) {
    throw new ManagedAuthorizationError(
      "grant_binding_mismatch",
      "managed ensure request binding does not match the canonical request",
    );
  }
  requireNoBrokeredCredentials(binding);
  const hashes = await managedRequestHashes(
    request.canonical_request.execution,
    binding,
  );
  const claims = await authorizeExecutionGrant(
    env,
    authorization.grant,
    expectedBinding(env, "ensure_session", request.workspace_id, binding, hashes),
  );
  return claims.installation.execution_realm_id;
}

/** Fresh authorization is checked before every D1 retry/result lookup. */
export async function authorizeManagedInvoke(
  env: Env,
  request: InvokeSessionRequest,
): Promise<`sha256:${string}` | undefined> {
  const authorization = request.managed_authorization;
  if (!authorization) {
    if (managedAuthRequired(env, request.execution !== undefined)) {
      throw new ManagedAuthorizationError(
        "invalid_grant",
        "managed execution grant is required",
      );
    }
    return undefined;
  }
  const { managed_authorization: _authorization, ...independentRequest } = request;
  rejectCredentialMaterial(independentRequest, "invoke_request");
  const binding = authorization.request_binding;
  if (
    binding.invocation_id !== request.invocation_id ||
    binding.session_idempotency_key !== request.invocation_id ||
    binding.delivery_receipt_id !== request.delivery_receipt_id ||
    binding.rendered_input_hash !== request.rendered_input_hash ||
    binding.principal_id !==
      requiredInvoke(request.activity_principal_id, "activity_principal_id") ||
    binding.policy_id !== requiredInvoke(request.policy_id, "policy_id") ||
    binding.run_id !== requiredInvoke(request.run_id, "run_id") ||
    binding.packet_id !== requiredInvoke(request.packet_id, "packet_id") ||
    binding.execution_policy_revision !==
      requiredInvoke(
        request.execution_policy_revision,
        "execution_policy_revision",
      ) ||
    request.session_ref.realm?.execution_realm_id !== env.PILLBOX_EXECUTION_REALM_ID
  ) {
    throw new ManagedAuthorizationError(
      "grant_binding_mismatch",
      "managed invoke request does not match its request binding",
    );
  }
  if (
    managedCanonicalJson(binding.output_format) !==
    managedCanonicalJson(request.output_format)
  ) {
    throw new ManagedAuthorizationError(
      "grant_binding_mismatch",
      "managed invoke output contract does not match its request binding",
    );
  }
  requireNoBrokeredCredentials(binding);
  const hashes = await managedRequestHashes(request.execution, binding);
  await authorizeExecutionGrant(
    env,
    authorization.grant,
    expectedBinding(env, "invoke_session", request.workspace_id, binding, hashes),
  );
  return `sha256:${await sha256Hex(managedCanonicalJson(binding))}`;
}

function managedAuthRequired(env: Env, currentEnvelope: boolean): boolean {
  if (env.MANAGED_AUTH_REQUIRED === "1") return true;
  return env.PillboxAuthorizationControlPlane !== undefined && currentEnvelope;
}

function requireNoBrokeredCredentials(binding: PillboxManagedRequestBinding): void {
  if (binding.runtime_policy.credential_bindings.length > 0) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      "managed credential bindings are disabled until the broker uses bounded non-DO storage",
    );
  }
}

function expectedBinding(
  env: Env,
  operation: "ensure_session" | "invoke_session",
  workspaceId: string,
  binding: PillboxManagedRequestBinding,
  hashes: {
    readonly execution_identity_hash: `sha256:${string}`;
    readonly output_contract_hash: `sha256:${string}`;
  },
): PillboxExecutionGrantBinding {
  const organizationId = env.PILLBOX_ORGANIZATION_ID;
  if (!organizationId) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      "managed organization pin is not configured",
    );
  }
  return {
    operation,
    installation: deploymentInstallation(env),
    organization_id: organizationId,
    workspace_id: workspaceId,
    principal_id: binding.principal_id,
    policy_id: binding.policy_id,
    run_id: binding.run_id,
    invocation_id: binding.invocation_id,
    packet_id: binding.packet_id,
    delivery_receipt_id: binding.delivery_receipt_id,
    session_idempotency_key: binding.session_idempotency_key,
    rendered_input_hash: binding.rendered_input_hash,
    execution_identity_hash: hashes.execution_identity_hash,
    output_contract_hash: hashes.output_contract_hash,
    runtime_policy: binding.runtime_policy,
  };
}

function deploymentInstallation(env: Env): PillboxInstallationRef {
  if (
    !env.PILLBOX_INSTALLATION_ID ||
    !env.PILLBOX_EXECUTION_REALM_ID ||
    env.PILLBOX_PROTOCOL_REVISION !== "pillbox.huddles/1"
  ) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      "managed deployment pins are not configured",
    );
  }
  return {
    installation_id: env.PILLBOX_INSTALLATION_ID,
    execution_realm_id: env.PILLBOX_EXECUTION_REALM_ID,
    protocol_revision: "pillbox.huddles/1",
  };
}

async function managedRequestHashes(
  execution: unknown,
  binding: PillboxManagedRequestBinding,
): Promise<{
  readonly execution_identity_hash: `sha256:${string}`;
  readonly output_contract_hash: `sha256:${string}`;
}> {
  if (execution === undefined) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      "managed request execution is unavailable",
    );
  }
  try {
    return {
      execution_identity_hash: `sha256:${await sha256Hex(
        managedCanonicalJson({
          execution,
          execution_policy_revision: binding.execution_policy_revision,
        }),
      )}`,
      output_contract_hash: `sha256:${await sha256Hex(
        managedCanonicalJson(binding.output_format),
      )}`,
    };
  } catch (cause) {
    throw new ManagedAuthorizationError(
      "grant_binding_mismatch",
      "managed request hash material is invalid",
      cause,
    );
  }
}

function rejectCredentialMaterial(value: unknown, path: string): void {
  if (Array.isArray(value)) {
    value.forEach((item, index) =>
      rejectCredentialMaterial(item, `${path}[${index}]`),
    );
    return;
  }
  if (!value || typeof value !== "object") return;
  for (const [key, child] of Object.entries(value)) {
    if (
      /(?:access|refresh)[_-]?token|api[_-]?key|authorization|cookie|client[_-]?secret|private[_-]?key/i.test(
        key,
      )
    ) {
      throw new ManagedAuthorizationError(
        "grant_binding_mismatch",
        `${path}.${key} cannot carry credential material`,
      );
    }
    rejectCredentialMaterial(child, `${path}.${key}`);
  }
}

function required(value: unknown, field: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      `Huddles request lacks canonical_request.${field}`,
    );
  }
  return value;
}

function requiredInvoke(value: unknown, field: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new ManagedAuthorizationError(
      "authorization_unavailable",
      `Huddles invoke request lacks ${field}`,
    );
  }
  return value;
}
