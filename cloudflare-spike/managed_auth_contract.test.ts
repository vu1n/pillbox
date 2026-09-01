import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { ManagedBurninBootstrapEnvironment } from "./src/managed_auth.ts";

registerHooks({
  resolve(specifier, context, nextResolve) {
    return nextResolve(
      context.parentURL?.includes("/cloudflare-spike/src/") &&
        specifier.startsWith(".") &&
        specifier.endsWith(".js")
        ? `${specifier.slice(0, -3)}.ts`
        : specifier,
      context,
    );
  },
});

const {
  executionOperationRequestDigest,
  requireManagedBurninBootstrap,
  requireManagedBurninHttpBootstrap,
} = await import("./src/managed_auth.ts");
import {
  managedCanonicalJson,
  makeExecutionOperationGrantCurrentnessRequest,
  validateExecutionOperationBinding,
  validateExecutionOperationGrantClaims,
  validateExecutionOperationGrantIssueRequest,
  validateExecutionOperationGrantIssueResponse,
} from "./src/managed_contract.ts";

const digest = "sha256:" + "a".repeat(64);
const operationClaims = {
  version: "huddles.execution-operation-grant/2" as const,
  grant_id: "operation-grant-1",
  installation: {
    installation_id: "install-1",
    execution_realm_id: "realm-1",
    protocol_revision: "pillbox.huddles/1" as const,
  },
  organization_id: "org-1",
  workspace_id: "ws-1",
  principal_id: "principal-1",
  policy_id: "policy-1",
  operation: "execute" as const,
  invocation_id: "inv-1",
  request_digest: digest,
  issued_at: 100,
  not_before: 100,
  expires_at: 160,
};

test("operation grants mirror the strict Huddles v2 and currentness v3 contracts", () => {
  const claims = validateExecutionOperationGrantClaims(operationClaims);
  const authorization = validateExecutionOperationGrantIssueResponse({
    grant: {
      algorithm: "Ed25519",
      key_id: "key-1",
      claims,
      signature: "signature-1",
    },
  });
  const expected = {
    operation: "execute" as const,
    invocation_id: "inv-1",
    request_digest: digest,
  };
  const currentness = makeExecutionOperationGrantCurrentnessRequest(
    authorization.grant,
    expected,
    { algorithm: "Ed25519", key_id: "key-1", public_key_sha256: digest },
  );

  assert.equal(managedCanonicalJson({ b: 1, a: 2 }), '{"a":2,"b":1}');
  assert.equal(currentness.version, "pillbox.authorization-currentness/3");
  assert.deepEqual(currentness.grant, authorization.grant);
  assert.equal(validateExecutionOperationBinding(claims, expected), undefined);
  assert.equal(
    validateExecutionOperationBinding(claims, { ...expected, operation: "cancel" }),
    "operation_mismatch",
  );
  assert.equal(
    validateExecutionOperationBinding(claims, { ...expected, invocation_id: "other" }),
    "invocation_mismatch",
  );
  assert.equal(
    validateExecutionOperationBinding(claims, {
      ...expected,
      request_digest: `sha256:${"b".repeat(64)}`,
    }),
    "request_digest_mismatch",
  );
  assert.throws(
    () => validateExecutionOperationGrantClaims({ ...claims, operations: ["execute"] }),
    /unrecognized field/,
  );
  assert.throws(
    () => validateExecutionOperationGrantIssueResponse({
      ...authorization,
      request_binding: expected,
    }),
    /unrecognized field/,
  );
});

test("operation grant issue requests are operation-scoped and bounded", () => {
  const issue = validateExecutionOperationGrantIssueRequest({
    grant_id: "operation-grant-1",
    installation_id: "install-1",
    workspace_id: "ws-1",
    principal_id: "principal-1",
    policy_id: "policy-1",
    operation: "status",
    invocation_id: "inv-1",
    request_digest: digest,
    ttl_seconds: 30,
  });
  assert.equal(issue.operation, "status");
  assert.throws(
    () => validateExecutionOperationGrantIssueRequest({ ...issue, ttl_seconds: 301 }),
    /ttl_seconds/,
  );
});

test("operation authorization digest binds the canonical invocation idempotency key", async () => {
  const request = {
    contract_version: "pillbox.execution/2",
    invocation_id: "inv-1",
    idempotency_key: "inv-1",
    reason: "operator requested cancellation",
  } as const;
  const canonical = await executionOperationRequestDigest(request);
  const noncanonical = await executionOperationRequestDigest({
    ...request,
    idempotency_key: "cancel-delivery-1",
  });

  assert.match(canonical, /^sha256:[0-9a-f]{64}$/);
  assert.notEqual(noncanonical, canonical);
});

test("burn-in runtime bootstrap rejects incomplete deployment pins before execution", () => {
  const complete: ManagedBurninBootstrapEnvironment = {
    PILLBOX_BOOTSTRAP_REQUIRED: "1",
    MANAGED_CAPABILITY_SECRET: "s".repeat(32),
    PILLBOX_GRANT_KEY_ID: "huddles-pillbox-grant-burnin-1",
    PILLBOX_GRANT_PUBLIC_KEY: "ed25519:SKaA_nE69844nq00przjwmcPcg5iRY1hL4fnfgipY94",
    PILLBOX_INSTALLATION_ID: "pillbox-burnin-installation-1",
    PILLBOX_EXECUTION_REALM_ID: "pillbox-managed-burnin-1",
    PILLBOX_PROTOCOL_REVISION: "pillbox.huddles/1",
    PILLBOX_ORGANIZATION_ID: "pillbox-burnin-organization-1",
    PILLBOX_MANAGED_CONCURRENCY: "1",
    MANAGED_EXECUTION_EPOCH: "burnin-2026-09-01-v1",
    MANAGED_EXECUTION_LIMIT: "1",
    PillboxAuthorizationCurrentness: {
      authorizeExecutionOperationGrant: async () => operationClaims,
    },
  };

  assert.doesNotThrow(() => requireManagedBurninBootstrap(complete));
  assert.doesNotThrow(() => requireManagedBurninHttpBootstrap(complete));
  const withoutHttpSecret = {
    ...complete,
    MANAGED_CAPABILITY_SECRET: undefined,
  };
  assert.doesNotThrow(() => requireManagedBurninBootstrap(withoutHttpSecret));
  assert.throws(
    () => requireManagedBurninHttpBootstrap(withoutHttpSecret),
    /capability secret is not configured/,
  );
  assert.throws(
    () =>
      requireManagedBurninBootstrap({
        ...complete,
        PILLBOX_GRANT_PUBLIC_KEY: "ed25519:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
      }),
    /grant public key is invalid/,
  );
  assert.throws(
    () =>
      requireManagedBurninBootstrap({
        ...complete,
        PILLBOX_MANAGED_CONCURRENCY: "2",
      }),
    /concurrency pins do not match/,
  );
  assert.throws(
    () =>
      requireManagedBurninBootstrap({
        ...complete,
        PillboxAuthorizationCurrentness: undefined,
      }),
    /currentness service is not configured/,
  );
});

test("Worker runs the burn-in bootstrap gate before request routing", async () => {
  const worker = await readFile(new URL("./src/worker.ts", import.meta.url), "utf8");
  const fetchStart = worker.indexOf("async fetch(req: Request, env: Env)");
  const bootstrap = worker.indexOf("requireManagedBurninHttpBootstrap(env)", fetchStart);
  const execution = worker.indexOf("routeExecutionRequest(req, env)", fetchStart);
  const workspace = worker.indexOf("routeWorkspaceTransfer(req, env)", fetchStart);

  assert.ok(fetchStart >= 0);
  assert.ok(bootstrap > fetchStart);
  assert.ok(execution > bootstrap);
  assert.ok(workspace > execution);
});
