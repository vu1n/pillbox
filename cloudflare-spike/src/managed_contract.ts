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

export type PillboxCredentialBindingRef = {
  readonly credential_binding_id: string;
  readonly secret_ref: string;
  readonly purpose: string;
};

export type PillboxRuntimePolicy = {
  readonly revision: string;
  readonly tool_policy: "deny_all";
  readonly credential_bindings: readonly PillboxCredentialBindingRef[];
  readonly egress: "credential_hosts_only";
};

/**
 * Unsigned request-side facts supplied by Huddles beside a signed grant.
 *
 * These values are deliberately not copied from the grant. Pillbox checks
 * them against the request, recomputes the hash fields from the request's
 * execution/output values, and only then asks Huddles to authorize the exact
 * expected binding.
 */
export type PillboxManagedOutputFormat = {
  readonly type: "json_schema";
  readonly schema: Record<string, unknown>;
  readonly retry_count: 2;
};

export type PillboxManagedRequestBinding = {
  readonly principal_id: string;
  readonly policy_id: string;
  readonly run_id: string;
  readonly invocation_id: string;
  readonly packet_id: string;
  readonly delivery_receipt_id: string;
  readonly session_idempotency_key: string;
  readonly rendered_input_hash: `sha256:${string}`;
  readonly execution_policy_revision: string;
  readonly output_format: PillboxManagedOutputFormat;
  readonly runtime_policy: PillboxRuntimePolicy;
};

export type PillboxManagedAuthorization = {
  readonly grant: SignedPillboxExecutionGrant;
  readonly request_binding: PillboxManagedRequestBinding;
};

export type PillboxGrantOperation = "ensure_session" | "invoke_session";

export type PillboxExecutionGrantClaims = {
  readonly version: "huddles.execution-grant/1";
  readonly grant_id: string;
  readonly installation: PillboxInstallationRef;
  readonly organization_id: string;
  readonly workspace_id: string;
  readonly policy: { readonly principal_id: string; readonly policy_id: string };
  readonly operations: readonly PillboxGrantOperation[];
  readonly run_id: string;
  readonly invocation_id: string;
  readonly packet_id: string;
  readonly delivery_receipt_id: string;
  readonly session_idempotency_key: string;
  readonly rendered_input_hash: `sha256:${string}`;
  readonly execution_identity_hash: `sha256:${string}`;
  readonly output_contract_hash: `sha256:${string}`;
  readonly runtime_policy: PillboxRuntimePolicy;
  readonly issued_at: number;
  readonly not_before: number;
  readonly expires_at: number;
};

export type SignedPillboxExecutionGrant = {
  readonly algorithm: "Ed25519";
  readonly key_id: string;
  readonly claims: PillboxExecutionGrantClaims;
  readonly signature: string;
};

export type ExecutionRealmRef = {
  readonly runtime: "pillbox";
  readonly execution_realm_id: string;
};

export type PillboxSessionRef = {
  readonly realm: ExecutionRealmRef;
  readonly session_id: string;
  readonly seq?: number;
  readonly seq_range?: readonly [number, number];
  readonly event_cursor?: string;
  readonly snapshot_ref?: string;
};

export type PillboxEvidenceReadGrantClaims = {
  readonly version: "huddles.evidence-read-grant/1";
  readonly grant_id: string;
  readonly installation: PillboxInstallationRef;
  readonly workspace_id: string;
  readonly viewer_principal_id: string;
  readonly policy_id: string;
  readonly run_id: string;
  readonly session_ref: PillboxSessionRef;
  readonly issued_at: number;
  readonly not_before: number;
  readonly expires_at: number;
};

export type SignedPillboxEvidenceReadGrant = {
  readonly algorithm: "Ed25519";
  readonly key_id: string;
  readonly claims: PillboxEvidenceReadGrantClaims;
  readonly signature: string;
};

export type PillboxExecutionGrantBinding = {
  readonly operation: PillboxGrantOperation;
  readonly installation: PillboxInstallationRef;
  readonly organization_id: string;
  readonly workspace_id: string;
  readonly principal_id: string;
  readonly policy_id: string;
  readonly run_id: string;
  readonly invocation_id: string;
  readonly packet_id: string;
  readonly delivery_receipt_id: string;
  readonly session_idempotency_key: string;
  readonly rendered_input_hash: string;
  readonly execution_identity_hash: string;
  readonly output_contract_hash: string;
  readonly runtime_policy: PillboxRuntimePolicy;
};

export type PillboxEvidenceReadBinding = {
  readonly installation: PillboxInstallationRef;
  readonly workspace_id: string;
  readonly viewer_principal_id: string;
  readonly policy_id: string;
  readonly run_id: string;
  readonly session_ref: PillboxSessionRef;
};

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

export function validateRuntimePolicy(value: unknown): PillboxRuntimePolicy {
  if (!record(value)) throw new ManagedContractError("runtime_policy must be an object");
  rejectUnknown(value, ["revision", "tool_policy", "credential_bindings", "egress"], "runtime_policy");
  if (value.tool_policy !== "deny_all" || value.egress !== "credential_hosts_only") {
    throw new ManagedContractError("managed runtime policy is not deny-all/credential-hosts-only");
  }
  if (!Array.isArray(value.credential_bindings)) {
    throw new ManagedContractError("runtime_policy.credential_bindings must be an array");
  }
  const bindings = value.credential_bindings.map((raw, index) => {
    if (!record(raw)) throw new ManagedContractError(`credential binding ${index} is invalid`);
    rejectUnknown(raw, ["credential_binding_id", "secret_ref", "purpose"], `credential_bindings[${index}]`);
    return {
      credential_binding_id: nonEmpty(raw.credential_binding_id, `credential_bindings[${index}].credential_binding_id`),
      secret_ref: nonEmpty(raw.secret_ref, `credential_bindings[${index}].secret_ref`),
      purpose: nonEmpty(raw.purpose, `credential_bindings[${index}].purpose`),
    };
  });
  const ids = new Set(bindings.map((binding) => binding.credential_binding_id));
  const refs = new Set(bindings.map((binding) => `${binding.secret_ref}\u0000${binding.purpose}`));
  if (ids.size !== bindings.length || refs.size !== bindings.length) {
    throw new ManagedContractError("credential bindings must be unique");
  }
  for (let index = 1; index < bindings.length; index += 1) {
    if (bindings[index - 1]!.credential_binding_id >= bindings[index]!.credential_binding_id) {
      throw new ManagedContractError("credential bindings must be sorted by ID");
    }
  }
  return { revision: nonEmpty(value.revision, "runtime_policy.revision"), tool_policy: "deny_all", credential_bindings: bindings, egress: "credential_hosts_only" };
}

export function validateManagedRequestBinding(value: unknown): PillboxManagedRequestBinding {
  if (!record(value)) throw new ManagedContractError("request_binding must be an object");
  rejectUnknown(value, ["principal_id", "policy_id", "run_id", "invocation_id", "packet_id", "delivery_receipt_id", "session_idempotency_key", "rendered_input_hash", "execution_policy_revision", "output_format", "runtime_policy"], "request_binding");
  return {
    principal_id: nonEmpty(value.principal_id, "request_binding.principal_id"),
    policy_id: nonEmpty(value.policy_id, "request_binding.policy_id"),
    run_id: nonEmpty(value.run_id, "request_binding.run_id"),
    invocation_id: nonEmpty(value.invocation_id, "request_binding.invocation_id"),
    packet_id: nonEmpty(value.packet_id, "request_binding.packet_id"),
    delivery_receipt_id: nonEmpty(value.delivery_receipt_id, "request_binding.delivery_receipt_id"),
    session_idempotency_key: nonEmpty(value.session_idempotency_key, "request_binding.session_idempotency_key"),
    rendered_input_hash: digest(value.rendered_input_hash, "request_binding.rendered_input_hash"),
    execution_policy_revision: nonEmpty(value.execution_policy_revision, "request_binding.execution_policy_revision"),
    output_format: outputFormat(value.output_format),
    runtime_policy: validateRuntimePolicy(value.runtime_policy),
  };
}

function outputFormat(value: unknown): PillboxManagedOutputFormat {
  if (!record(value)) throw new ManagedContractError("request_binding.output_format must be an object");
  rejectUnknown(value, ["type", "schema", "retry_count"], "request_binding.output_format");
  if (value.type !== "json_schema" || value.retry_count !== 2 || !record(value.schema)) {
    throw new ManagedContractError("request_binding.output_format is invalid");
  }
  assertJson(value.schema, "request_binding.output_format.schema");
  return { type: "json_schema", schema: value.schema, retry_count: 2 };
}

function assertJson(value: unknown, path: string): void {
  if (value === null || typeof value === "boolean" || typeof value === "string") return;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new ManagedContractError(`${path} contains a non-finite number`);
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((item, index) => assertJson(item, `${path}[${index}]`));
    return;
  }
  if (record(value)) {
    Object.entries(value).forEach(([key, item]) => assertJson(item, `${path}.${key}`));
    return;
  }
  throw new ManagedContractError(`${path} is not JSON-compatible`);
}

export function validateExecutionGrantClaims(value: unknown): PillboxExecutionGrantClaims {
  if (!record(value)) throw new ManagedContractError("grant claims must be an object");
  rejectUnknown(value, ["version", "grant_id", "installation", "organization_id", "workspace_id", "policy", "operations", "run_id", "invocation_id", "packet_id", "delivery_receipt_id", "session_idempotency_key", "rendered_input_hash", "execution_identity_hash", "output_contract_hash", "runtime_policy", "issued_at", "not_before", "expires_at"], "grant claims");
  if (value.version !== "huddles.execution-grant/1") throw new ManagedContractError("unsupported execution grant version");
  const operations = value.operations;
  if (!Array.isArray(operations) || operations.length === 0 || operations.some((operation) => operation !== "ensure_session" && operation !== "invoke_session")) {
    throw new ManagedContractError("grant operations are invalid");
  }
  if (new Set(operations).size !== operations.length) throw new ManagedContractError("grant operations are not unique");
  const issued = integer(value.issued_at, "issued_at");
  const notBefore = integer(value.not_before, "not_before");
  const expires = integer(value.expires_at, "expires_at");
  if (!(issued <= notBefore && notBefore < expires && expires - issued <= 300)) throw new ManagedContractError("grant time window is invalid");
  if (!record(value.policy)) throw new ManagedContractError("grant policy is invalid");
  rejectUnknown(value.policy, ["principal_id", "policy_id"], "grant policy");
  const claims: PillboxExecutionGrantClaims = {
    version: "huddles.execution-grant/1",
    grant_id: nonEmpty(value.grant_id, "grant_id"),
    installation: installation(value.installation),
    organization_id: nonEmpty(value.organization_id, "organization_id"),
    workspace_id: nonEmpty(value.workspace_id, "workspace_id"),
    policy: { principal_id: nonEmpty(value.policy.principal_id, "policy.principal_id"), policy_id: nonEmpty(value.policy.policy_id, "policy.policy_id") },
    operations: [...operations] as PillboxGrantOperation[],
    run_id: nonEmpty(value.run_id, "run_id"),
    invocation_id: nonEmpty(value.invocation_id, "invocation_id"),
    packet_id: nonEmpty(value.packet_id, "packet_id"),
    delivery_receipt_id: nonEmpty(value.delivery_receipt_id, "delivery_receipt_id"),
    session_idempotency_key: nonEmpty(value.session_idempotency_key, "session_idempotency_key"),
    rendered_input_hash: digest(value.rendered_input_hash, "rendered_input_hash"),
    execution_identity_hash: digest(value.execution_identity_hash, "execution_identity_hash"),
    output_contract_hash: digest(value.output_contract_hash, "output_contract_hash"),
    runtime_policy: validateRuntimePolicy(value.runtime_policy),
    issued_at: issued,
    not_before: notBefore,
    expires_at: expires,
  };
  return claims;
}

export function validateSignedExecutionGrant(value: unknown): SignedPillboxExecutionGrant {
  if (!record(value) || value.algorithm !== "Ed25519") throw new ManagedContractError("signed grant algorithm is invalid");
  rejectUnknown(value, ["algorithm", "key_id", "claims", "signature"], "signed grant");
  const signature = nonEmpty(value.signature, "grant.signature");
  if (!/^[A-Za-z0-9_-]+$/.test(signature)) throw new ManagedContractError("grant signature must be base64url");
  return { algorithm: "Ed25519", key_id: nonEmpty(value.key_id, "grant.key_id"), claims: validateExecutionGrantClaims(value.claims), signature };
}

export function validateSessionRef(value: unknown): PillboxSessionRef {
  if (!record(value) || !record(value.realm) || value.realm.runtime !== "pillbox") throw new ManagedContractError("managed session ref realm is invalid");
  rejectUnknown(value, ["realm", "session_id", "seq", "seq_range", "event_cursor", "snapshot_ref"], "session ref");
  rejectUnknown(value.realm, ["runtime", "execution_realm_id"], "session ref realm");
  const session: { -readonly [K in keyof PillboxSessionRef]?: PillboxSessionRef[K] } = { realm: { runtime: "pillbox", execution_realm_id: nonEmpty(value.realm.execution_realm_id, "session_ref.realm.execution_realm_id") }, session_id: nonEmpty(value.session_id, "session_ref.session_id") };
  if (value.seq !== undefined) session.seq = integer(value.seq, "session_ref.seq");
  if (value.seq_range !== undefined) {
    if (value.seq !== undefined) throw new ManagedContractError("session_ref cannot contain both seq and seq_range");
    if (!Array.isArray(value.seq_range) || value.seq_range.length !== 2) throw new ManagedContractError("session_ref.seq_range must have two values");
    const range: [number, number] = [integer(value.seq_range[0], "session_ref.seq_range[0]"), integer(value.seq_range[1], "session_ref.seq_range[1]")];
    if (range[0] > range[1]) throw new ManagedContractError("session_ref.seq_range must be ordered");
    session.seq_range = range;
  }
  if (value.event_cursor !== undefined) session.event_cursor = nonEmpty(value.event_cursor, "session_ref.event_cursor");
  if (value.snapshot_ref !== undefined) session.snapshot_ref = nonEmpty(value.snapshot_ref, "session_ref.snapshot_ref");
  return session as PillboxSessionRef;
}

/** The current reader supports only one positional sequence selector. */
export function validatePositionalEvidenceSelector(value: unknown): PillboxSessionRef {
  const session = validateSessionRef(value);
  if (session.seq === undefined && session.seq_range === undefined) throw new ManagedContractError("evidence reader requires a positional selector");
  if (session.event_cursor !== undefined || session.snapshot_ref !== undefined) throw new ManagedContractError("evidence reader does not support cursor or snapshot selectors");
  return session;
}

export function validateEvidenceReadClaims(value: unknown): PillboxEvidenceReadGrantClaims {
  if (!record(value) || value.version !== "huddles.evidence-read-grant/1") throw new ManagedContractError("evidence grant version is invalid");
  rejectUnknown(value, ["version", "grant_id", "installation", "workspace_id", "viewer_principal_id", "policy_id", "run_id", "session_ref", "issued_at", "not_before", "expires_at"], "evidence grant");
  const claims = { version: "huddles.evidence-read-grant/1", grant_id: nonEmpty(value.grant_id, "grant_id"), installation: installation(value.installation), workspace_id: nonEmpty(value.workspace_id, "workspace_id"), viewer_principal_id: nonEmpty(value.viewer_principal_id, "viewer_principal_id"), policy_id: nonEmpty(value.policy_id, "policy_id"), run_id: nonEmpty(value.run_id, "run_id"), session_ref: validateSessionRef(value.session_ref), issued_at: integer(value.issued_at, "issued_at"), not_before: integer(value.not_before, "not_before"), expires_at: integer(value.expires_at, "expires_at") } satisfies PillboxEvidenceReadGrantClaims;
  if (claims.session_ref.realm.execution_realm_id !== claims.installation.execution_realm_id || !(claims.issued_at <= claims.not_before && claims.not_before < claims.expires_at)) throw new ManagedContractError("evidence grant binding or time window is invalid");
  return claims;
}

function integer(value: unknown, field: string): number { if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new ManagedContractError(`${field} must be a non-negative integer`); return value; }
function record(value: unknown): value is Record<string, unknown> { return typeof value === "object" && value !== null && !Array.isArray(value); }

export function managedCanonicalJson(value: unknown): string {
  if (value === undefined || typeof value === "function" || typeof value === "symbol") throw new ManagedContractError("managed hash material must be JSON-compatible");
  if (value === null || typeof value !== "object") { if (typeof value === "number" && !Number.isFinite(value)) throw new ManagedContractError("managed hash material contains a non-finite number"); return JSON.stringify(value); }
  if (Array.isArray(value)) return `[${value.map(managedCanonicalJson).join(",")}]`;
  const object = value as Record<string, unknown>;
  return `{${Object.keys(object).sort().map((key) => `${JSON.stringify(key)}:${managedCanonicalJson(object[key])}`).join(",")}}`;
}

export function validateGrantBinding(claims: PillboxExecutionGrantClaims, expected: PillboxExecutionGrantBinding): string | undefined {
  const sameInstallation = claims.installation.installation_id === expected.installation.installation_id && claims.installation.execution_realm_id === expected.installation.execution_realm_id && claims.installation.protocol_revision === expected.installation.protocol_revision;
  const checks: [boolean, string][] = [
    [sameInstallation, "installation_mismatch"], [claims.organization_id === expected.organization_id, "organization_mismatch"], [claims.workspace_id === expected.workspace_id, "workspace_mismatch"], [claims.policy.principal_id === expected.principal_id, "principal_mismatch"], [claims.policy.policy_id === expected.policy_id, "policy_mismatch"], [claims.operations.includes(expected.operation), "operation_mismatch"], [claims.run_id === expected.run_id, "run_mismatch"], [claims.invocation_id === expected.invocation_id, "invocation_mismatch"], [claims.packet_id === expected.packet_id, "packet_mismatch"], [claims.delivery_receipt_id === expected.delivery_receipt_id, "delivery_receipt_mismatch"], [claims.session_idempotency_key === expected.session_idempotency_key, "session_idempotency_mismatch"], [claims.rendered_input_hash === expected.rendered_input_hash, "rendered_input_hash_mismatch"], [claims.execution_identity_hash === expected.execution_identity_hash, "execution_identity_hash_mismatch"], [claims.output_contract_hash === expected.output_contract_hash, "output_contract_hash_mismatch"], [managedCanonicalJson(claims.runtime_policy) === managedCanonicalJson(expected.runtime_policy), "runtime_policy_mismatch"],
  ];
  return checks.find(([matches]) => !matches)?.[1];
}

export function validateEvidenceBinding(claims: PillboxEvidenceReadGrantClaims, expected: PillboxEvidenceReadBinding): string | undefined {
  const sameSession = claims.session_ref.realm.execution_realm_id === expected.session_ref.realm.execution_realm_id && claims.session_ref.session_id === expected.session_ref.session_id;
  const requested = expected.session_ref;
  const requestedHasPosition = requested.seq !== undefined || requested.seq_range !== undefined || requested.event_cursor !== undefined || requested.snapshot_ref !== undefined;
  const allowedRange = requested.seq === undefined && requested.seq_range === undefined || claims.session_ref.seq !== undefined || claims.session_ref.seq_range !== undefined;
  const contained = requested.seq === undefined && requested.seq_range === undefined || requested.seq !== undefined && (claims.session_ref.seq !== undefined && requested.seq === claims.session_ref.seq || claims.session_ref.seq_range !== undefined && requested.seq >= claims.session_ref.seq_range[0] && requested.seq <= claims.session_ref.seq_range[1]) || requested.seq_range !== undefined && claims.session_ref.seq_range !== undefined && requested.seq_range[0] >= claims.session_ref.seq_range[0] && requested.seq_range[1] <= claims.session_ref.seq_range[1];
  const cursorsMatch = requested.event_cursor === undefined || requested.event_cursor === claims.session_ref.event_cursor;
  const snapshotsMatch = requested.snapshot_ref === undefined || requested.snapshot_ref === claims.session_ref.snapshot_ref;
  const checks: [boolean, string][] = [[claims.installation.installation_id === expected.installation.installation_id && claims.installation.execution_realm_id === expected.installation.execution_realm_id, "installation_mismatch"], [claims.workspace_id === expected.workspace_id, "workspace_mismatch"], [claims.viewer_principal_id === expected.viewer_principal_id, "viewer_principal_mismatch"], [claims.policy_id === expected.policy_id, "policy_mismatch"], [claims.run_id === expected.run_id, "run_mismatch"], [sameSession && requestedHasPosition && allowedRange && contained && cursorsMatch && snapshotsMatch, "session_mismatch"]];
  return checks.find(([matches]) => !matches)?.[1];
}

export function isManagedGrantCurrent(claims: { not_before: number; expires_at: number }, now: number, skewSeconds = 0): boolean { return Number.isSafeInteger(now) && now >= claims.not_before - skewSeconds && now < claims.expires_at + skewSeconds; }

function rejectUnknown(value: Record<string, unknown>, allowed: readonly string[], path: string): void {
  const permitted = new Set(allowed);
  const unknown = Object.keys(value).find((key) => !permitted.has(key));
  if (unknown) throw new ManagedContractError(`${path} contains unrecognized field '${unknown}'`);
}
