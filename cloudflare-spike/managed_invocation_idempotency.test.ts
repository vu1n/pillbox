import assert from "node:assert/strict";
import { test } from "node:test";
import { managedInvocationSemanticHash, managedInvocationSemanticJson } from "./src/managed_contract.ts";

const digest = "sha256:" + "a".repeat(64);

const requestBinding = {
  principal_id: "principal-1",
  policy_id: "policy-1",
  run_id: "run-1",
  invocation_id: "invocation-1",
  packet_id: "packet-1",
  delivery_receipt_id: "delivery-1",
  session_idempotency_key: "invocation-1",
  rendered_input_hash: digest,
  execution_policy_revision: "execution/1",
  output_format: { type: "json_schema", schema: { type: "object" }, retry_count: 2 },
  runtime_policy: { revision: "runtime/1", tool_policy: "deny_all", credential_bindings: [], egress: "credential_hosts_only" },
};

function request(grant: Record<string, unknown>, binding = requestBinding) {
  return {
    workspace_id: "workspace-1",
    effect_id: "effect-1",
    invocation_id: "invocation-1",
    session_ref: { session_id: "session-1" },
    delivery_receipt_id: "delivery-1",
    rendered_input: "sealed input",
    rendered_input_hash: digest,
    tool_policy: "deny_all",
    requested_model: "provider/model",
    execution: { placement: "managed_container", transport: { harness: "opencode" } },
    output_format: { type: "json_schema", schema: { type: "object" }, retry_count: 2 },
    managed_authorization: { grant, request_binding: binding },
  };
}

test("semantic invocation identity ignores replacement grant envelope volatility", async () => {
  const first = request({ grant_id: "grant-a", key_id: "key-a", signature: "signature-a", issued_at: 100, expires_at: 160 });
  const replacement = request({ grant_id: "grant-b", key_id: "key-b", signature: "signature-b", issued_at: 200, expires_at: 260 });

  assert.equal(
    managedInvocationSemanticJson(first),
    managedInvocationSemanticJson(replacement),
  );
  assert.equal(
    await managedInvocationSemanticHash(first),
    await managedInvocationSemanticHash(replacement),
  );
});

test("semantic invocation identity retains immutable request-tuple conflict protection", async () => {
  const original = request({ grant_id: "grant-a", key_id: "key-a", signature: "signature-a" });
  const changedBinding = request(
    { grant_id: "grant-b", key_id: "key-b", signature: "signature-b" },
    { ...requestBinding, delivery_receipt_id: "delivery-2" },
  );

  assert.notEqual(
    await managedInvocationSemanticHash(original),
    await managedInvocationSemanticHash(changedBinding),
  );
});

test("legacy PR #145 request rows normalize without rewriting stored history", async () => {
  const legacy = request({ grant_id: "old-grant", key_id: "old-key", signature: "old-signature", issued_at: 100, expires_at: 160 });
  const retry = request({ grant_id: "fresh-grant", key_id: "fresh-key", signature: "fresh-signature", issued_at: 200, expires_at: 260 });

  assert.equal(
    await managedInvocationSemanticHash(legacy),
    await managedInvocationSemanticHash(retry),
  );
  assert.match(managedInvocationSemanticJson(legacy), /request_binding/);
  assert.doesNotMatch(managedInvocationSemanticJson(legacy), /old-signature/);
});

test("malformed persisted authorization material fails closed", () => {
  assert.throws(
    () => managedInvocationSemanticJson({ managed_authorization: null }),
    /authorization binding is unavailable/,
  );
});
