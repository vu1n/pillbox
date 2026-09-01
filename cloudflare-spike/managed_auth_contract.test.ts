import assert from "node:assert/strict";
import { test } from "node:test";
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
