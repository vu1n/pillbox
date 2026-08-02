import assert from "node:assert/strict";
import { test } from "node:test";
import { isCurrentRefreshGeneration } from "./src/credentials/contract.ts";
import { managedCanonicalJson, validateExecutionGrantClaims, validateGrantBinding, validateManagedRequestBinding } from "./src/managed_contract.ts";
import { authorizeOutboundRequest, safeProviderRedirect, scrubCredentialResponseHeaders } from "./src/outbound/policy.ts";

const digest = "sha256:" + "a".repeat(64);
const claims = {
  version: "huddles.execution-grant/1" as const,
  grant_id: "grant-1",
  installation: { installation_id: "install-1", execution_realm_id: "realm-1", protocol_revision: "pillbox.huddles/1" as const },
  organization_id: "org-1",
  workspace_id: "ws-1",
  policy: { principal_id: "principal-1", policy_id: "policy-1" },
  operations: ["ensure_session", "invoke_session"] as const,
  run_id: "run-1",
  invocation_id: "inv-1",
  packet_id: "packet-1",
  delivery_receipt_id: "delivery-1",
  session_idempotency_key: "effect-1",
  rendered_input_hash: digest,
  execution_identity_hash: digest,
  output_contract_hash: digest,
  runtime_policy: { revision: "pillbox-runtime/deny-all/1", tool_policy: "deny_all" as const, credential_bindings: [], egress: "credential_hosts_only" as const },
  issued_at: 100,
  not_before: 100,
  expires_at: 160,
};

test("managed grant claims are strict, canonical, and bind operation identity", () => {
  const decoded = validateExecutionGrantClaims(claims);
  assert.equal(managedCanonicalJson({ b: 1, a: 2 }), '{"a":2,"b":1}');
  assert.equal(validateGrantBinding(decoded, {
    operation: "invoke_session",
    installation: decoded.installation,
    organization_id: decoded.organization_id,
    workspace_id: decoded.workspace_id,
    principal_id: decoded.policy.principal_id,
    policy_id: decoded.policy.policy_id,
    run_id: decoded.run_id,
    invocation_id: decoded.invocation_id,
    packet_id: decoded.packet_id,
    delivery_receipt_id: decoded.delivery_receipt_id,
    session_idempotency_key: decoded.session_idempotency_key,
    rendered_input_hash: decoded.rendered_input_hash,
    execution_identity_hash: decoded.execution_identity_hash,
    output_contract_hash: decoded.output_contract_hash,
    runtime_policy: decoded.runtime_policy,
  }), undefined);
  assert.throws(() => validateExecutionGrantClaims({ ...claims, expires_at: 500 }), /time window/);
  assert.doesNotThrow(() => validateExecutionGrantClaims({ ...claims, operations: ["invoke_session"] }));
  assert.throws(() => validateExecutionGrantClaims({ ...claims, extra: true }), /unrecognized field/);
});

test("managed outbound policy is exact-host HTTPS and rejects credential-bearing URLs", () => {
  const route = { credential_binding_id: "binding", secret_ref: "secret", purpose: "provider", host: "api.example.test" };
  authorizeOutboundRequest({ request: new Request("https://api.example.test/v1"), route });
  authorizeOutboundRequest({ request: new Request("https://api.example.test:443/v1"), route });
  assert.throws(() => authorizeOutboundRequest({ request: new Request("http://api.example.test/v1"), route }));
  assert.throws(() => authorizeOutboundRequest({ request: new Request("https://api.example.test:444/v1"), route }));
  assert.throws(() => authorizeOutboundRequest({ request: new Request("https://evil.example.test/v1"), route }));
});

test("managed request binding is strict and remains independent from signed claims", () => {
  const binding = validateManagedRequestBinding({
    principal_id: "principal-1",
    policy_id: "policy-1",
    run_id: "run-1",
    invocation_id: "inv-1",
    packet_id: "packet-1",
    delivery_receipt_id: "delivery-1",
    session_idempotency_key: "inv-1",
    rendered_input_hash: digest,
    execution_policy_revision: "planning-actions/1",
    output_format: { type: "json_schema", schema: { type: "object" }, retry_count: 2 },
    runtime_policy: { revision: "runtime/1", tool_policy: "deny_all", credential_bindings: [], egress: "credential_hosts_only" },
  });
  assert.equal(binding.policy_id, "policy-1");
  assert.throws(() => validateManagedRequestBinding({ ...binding, extra: true }), /unrecognized field/);
  assert.throws(() => validateManagedRequestBinding({ ...binding, output_format: { ...binding.output_format, retry_count: 1 } }), /output_format/);
});

test("managed outbound redirects stay on the exact provider origin", () => {
  assert.equal(safeProviderRedirect({ location: "/next", baseUrl: "https://api.example.test/v1", routeHost: "api.example.test" }), "https://api.example.test/next");
  assert.equal(safeProviderRedirect({ location: "https://api.example.test:444/next", baseUrl: "https://api.example.test/v1", routeHost: "api.example.test" }), undefined);
  assert.equal(safeProviderRedirect({ location: "https://evil.example.test/next", baseUrl: "https://api.example.test/v1", routeHost: "api.example.test" }), undefined);
});

test("managed response scrubbing removes adjacent credential-bearing headers", () => {
  const scrubbed = scrubCredentialResponseHeaders({ headers: new Headers({ "x-first": "access-secret", "x-second": "access-secret", "www-authenticate": "Bearer challenge" }), accessToken: "access-secret" });
  assert.equal(scrubbed.has("x-first"), false);
  assert.equal(scrubbed.has("x-second"), false);
  assert.equal(scrubbed.has("www-authenticate"), false);
});

test("refresh re-auth terminalization is scoped to the expected generation", () => {
  assert.equal(isCurrentRefreshGeneration({ generation: 4, pending_generation: 5, status: "active" }, 4), true);
  assert.equal(isCurrentRefreshGeneration({ generation: 5, pending_generation: 6, status: "active" }, 4), false);
  assert.equal(isCurrentRefreshGeneration({ generation: 4, pending_generation: 5, status: "reauth_required" }, 4), false);
});
