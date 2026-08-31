import assert from "node:assert/strict";
import { test } from "node:test";
import {
  computeExecutionIdentityDigest,
  computeInvocationRequestHash,
  computeRenderedInputHash,
  CodexExecutionBoundaryError,
  MAX_EVIDENCE_PAGE_SIZE,
  type ExecuteInvocationV2Request,
  type InvocationExecution,
  UnsupportedAcpExecutionError,
  UnsupportedCodexExecutionError,
  validateExecuteInvocationV2Request,
  validateCancelInvocationV2Request,
  validateGetInvocationV2Request,
  validateSupportedAcpExecution,
  validateSupportedCodexExecution,
} from "./src/codex_execution.ts";
import { sha256Hex } from "./src/runtime_identity.ts";

const codexExecution: InvocationExecution = {
  transport: {
    harness: "codex",
    transport: "app_server",
    harness_version: "0.144.5",
    adapter_revision: "pillbox/p56-003",
  },
  requested: {
    provider: "openai-codex",
    model: "gpt-5.6-sol",
    profile: "sol",
    reasoning_effort: "high",
  },
  placement: "managed_container",
  context_renderer_revision: "planning-partner/cc-011",
  verifier_ref: "huddles/verifier/1",
};

const output_format = {
  type: "json_schema" as const,
  schema: {
    type: "object",
    properties: { ok: { type: "boolean" } },
    required: ["ok"],
    additionalProperties: false,
  },
  retry_count: 2 as const,
};

async function validRequest(
  changes: Partial<ExecuteInvocationV2Request> = {},
): Promise<ExecuteInvocationV2Request> {
  const rendered_input = changes.rendered_input ?? "Produce GRILL.md.";
  const execution = changes.execution ?? codexExecution;
  return {
    contract_version: "pillbox.execution/2",
    session_ref: { session_id: "session-1" },
    invocation_id: "invocation-1",
    idempotency_key: "delivery-1",
    rendered_input,
    rendered_input_hash: await computeRenderedInputHash(rendered_input),
    tool_policy: "deny_all",
    execution,
    execution_policy_revision:
      changes.execution_policy_revision ?? "planning-execution/codex-app-server/1",
    output_format: changes.output_format ?? output_format,
    ...changes,
  };
}

function assertBoundaryError(error: unknown): boolean {
  return (
    error instanceof CodexExecutionBoundaryError &&
    error.code === "invalid_execute_invocation_v2_request"
  );
}

function assertUnsupported(error: unknown): boolean {
  return (
    error instanceof UnsupportedCodexExecutionError &&
    error.code === "unsupported_execution"
  );
}

function assertUnsupportedAcp(error: unknown): boolean {
  return (
    error instanceof UnsupportedAcpExecutionError &&
    error.code === "unsupported_execution"
  );
}

test("valid Codex app-server execution envelope validates", async () => {
  const request = await validRequest();
  const validated = await validateExecuteInvocationV2Request(request);
  assert.deepEqual(validated, request);
  assert.deepEqual(validateSupportedCodexExecution(validated.execution), codexExecution);
});

test("single-controller CLI turns may use runtime tools and text output", async () => {
  const request = await validRequest({
    tool_policy: "runtime_default",
    output_format: { type: "text", retry_count: 0 },
    execution: {
      ...codexExecution,
      transport: {
        ...codexExecution.transport,
        harness: "opencode",
        transport: "http",
      },
    },
  });
  assert.deepEqual(await validateExecuteInvocationV2Request(request), request);
});

test("broad non-Codex execution validates, then fails the Codex capability check", async () => {
  const request = await validRequest({
    execution: {
      ...codexExecution,
      transport: {
        ...codexExecution.transport,
        harness: "opencode",
        transport: "http",
      },
    },
  });
  const validated = await validateExecuteInvocationV2Request(request);
  assert.equal(validated.execution.transport.harness, "opencode");
  assert.throws(
    () => validateSupportedCodexExecution(validated.execution),
    assertUnsupported,
  );
});

test("ACP is a separate generic capability beside native Codex app-server", async () => {
  const request = await validRequest({
    execution: {
      ...codexExecution,
      transport: {
        ...codexExecution.transport,
        transport: "acp",
      },
    },
  });
  const validated = await validateExecuteInvocationV2Request(request);
  assert.deepEqual(validateSupportedAcpExecution(validated.execution), validated.execution);
  assert.throws(
    () => validateSupportedAcpExecution(codexExecution),
    assertUnsupportedAcp,
  );
  assert.notEqual(
    await computeExecutionIdentityDigest(codexExecution, "policy/1"),
    await computeExecutionIdentityDigest(validated.execution, "policy/1"),
  );
});

test("execution identity digest is deterministic across object key order", async () => {
  const reordered: InvocationExecution = {
    context_renderer_revision: codexExecution.context_renderer_revision,
    verifier_ref: codexExecution.verifier_ref,
    requested: {
      reasoning_effort: codexExecution.requested.reasoning_effort,
      profile: codexExecution.requested.profile,
      model: codexExecution.requested.model,
      provider: codexExecution.requested.provider,
    },
    placement: codexExecution.placement,
    transport: {
      adapter_revision: codexExecution.transport.adapter_revision,
      harness_version: codexExecution.transport.harness_version,
      transport: codexExecution.transport.transport,
      harness: codexExecution.transport.harness,
    },
  };
  assert.equal(
    await computeExecutionIdentityDigest(codexExecution, "policy/1"),
    await computeExecutionIdentityDigest(reordered, "policy/1"),
  );
});

test("request hash changes with input, output, policy, and controller context", async () => {
  const request = await validRequest();
  const original = await computeInvocationRequestHash(request);
  const changedInput = await computeInvocationRequestHash(
    await validRequest({ rendered_input: "Produce a different file." }),
  );
  const changedOutput = await computeInvocationRequestHash(
    await validRequest({
      output_format: {
        ...output_format,
        schema: { ...output_format.schema, required: [] },
      },
    }),
  );
  const changedPolicy = await computeInvocationRequestHash(
    await validRequest({ execution_policy_revision: "planning-execution/codex-app-server/2" }),
  );
  const changedControllerContext = await computeInvocationRequestHash(
    await validRequest({ controller_context_hash: `sha256:${"b".repeat(64)}` }),
  );
  assert.notEqual(original, changedInput);
  assert.notEqual(original, changedOutput);
  assert.notEqual(original, changedPolicy);
  assert.notEqual(original, changedControllerContext);
});

test("rendered input hash is recomputed over the exact UTF-8 input", async () => {
  const request = await validRequest();
  await assert.rejects(
    validateExecuteInvocationV2Request({
      ...request,
      rendered_input_hash: `sha256:${"0".repeat(64)}`,
    }),
    assertBoundaryError,
  );
  const unicode = "Produce café 🐕.";
  const unicodeRequest = await validRequest({ rendered_input: unicode });
  assert.equal(
    unicodeRequest.rendered_input_hash,
    await computeRenderedInputHash(unicode),
  );
  await validateExecuteInvocationV2Request(unicodeRequest);
  assert.equal(
    unicodeRequest.rendered_input_hash,
    `sha256:${await sha256Hex(unicode)}`,
  );
});

test("tool policy, output schema, and retry count are sealed", async () => {
  const request = await validRequest();
  await assert.rejects(
    validateExecuteInvocationV2Request({ ...request, tool_policy: "allow_all" }),
    assertBoundaryError,
  );
  await assert.rejects(
    validateExecuteInvocationV2Request({
      ...request,
      output_format: { ...request.output_format, retry_count: 1 },
    }),
    assertBoundaryError,
  );
  await assert.rejects(
    validateExecuteInvocationV2Request({
      ...request,
      output_format: { ...request.output_format, schema: [] },
    }),
    assertBoundaryError,
  );
});

test("malformed execution is distinct from valid-but-unsupported execution", async () => {
  const request = await validRequest();
  await assert.rejects(
    validateExecuteInvocationV2Request({
      ...request,
      execution: {
        ...request.execution,
        transport: { ...request.execution.transport, transport: "" },
      },
    }),
    assertBoundaryError,
  );
  const unsupported = await validateExecuteInvocationV2Request(
    await validRequest({
      execution: {
        ...request.execution,
        transport: { ...request.execution.transport, harness: "pi", transport: "stdio" },
      },
    }),
  );
  assert.throws(() => validateSupportedCodexExecution(unsupported.execution), assertUnsupported);
});

test("optional verifier_ref preserves absence and explicit empty string", async () => {
  const absent = await validRequest({
    execution: (() => {
      const { verifier_ref: _ignored, ...withoutVerifier } = codexExecution;
      return withoutVerifier;
    })(),
  });
  const validatedAbsent = await validateExecuteInvocationV2Request(absent);
  assert.equal(Object.hasOwn(validatedAbsent.execution, "verifier_ref"), false);

  const explicit = await validRequest({
    execution: { ...codexExecution, verifier_ref: "" },
  });
  const validatedExplicit = await validateExecuteInvocationV2Request(explicit);
  assert.equal(validatedExplicit.execution.verifier_ref, "");
});

test("unknown fields and credential or claim fields are rejected", async () => {
  const request = await validRequest();
  for (const field of [
    "execution_digest",
    "credential_ref",
    "workspace_id",
    "effect_id",
    "delivery_receipt_id",
    "claim",
    "mutex",
    "lock",
  ]) {
    await assert.rejects(
      validateExecuteInvocationV2Request({ ...request, [field]: "not-a-capability" }),
      assertBoundaryError,
    );
  }
  await assert.rejects(
    validateExecuteInvocationV2Request({ ...request, invocation_id: 1 }),
    assertBoundaryError,
  );
});

test("status reads default to one bounded evidence page", () => {
  assert.deepEqual(
    validateGetInvocationV2Request({
      contract_version: "pillbox.execution/2",
      invocation_id: "invocation-1",
    }),
    {
      contract_version: "pillbox.execution/2",
      invocation_id: "invocation-1",
      evidence_after: 0,
      evidence_limit: MAX_EVIDENCE_PAGE_SIZE,
    },
  );
  assert.deepEqual(
    validateGetInvocationV2Request({
      contract_version: "pillbox.execution/2",
      invocation_id: "invocation-1",
      evidence_after: 12,
      evidence_limit: 8,
    }),
    {
      contract_version: "pillbox.execution/2",
      invocation_id: "invocation-1",
      evidence_after: 12,
      evidence_limit: 8,
    },
  );
});

test("status evidence pagination rejects scans and malformed cursors", () => {
  for (const fields of [
    { evidence_after: -1 },
    { evidence_after: 1.5 },
    { evidence_limit: 0 },
    { evidence_limit: MAX_EVIDENCE_PAGE_SIZE + 1 },
  ]) {
    assert.throws(
      () =>
        validateGetInvocationV2Request({
          contract_version: "pillbox.execution/2",
          invocation_id: "invocation-1",
          ...fields,
        }),
      assertBoundaryError,
    );
  }
  assert.throws(
    () =>
      validateGetInvocationV2Request({
        contract_version: "pillbox.execution/2",
        invocation_id: "invocation-1",
        list_all: true,
      }),
    assertBoundaryError,
  );
});

test("cancellation is an exact idempotent runtime request", () => {
  const request = {
    contract_version: "pillbox.execution/2",
    invocation_id: "invocation-1",
    idempotency_key: "cancel-delivery-1",
    reason: "caller requested cancellation",
  } as const;
  assert.deepEqual(validateCancelInvocationV2Request(request), request);
  assert.throws(
    () => validateCancelInvocationV2Request({ ...request, actor: "human:1" }),
    assertBoundaryError,
  );
  assert.throws(
    () => validateCancelInvocationV2Request({ ...request, reason: "" }),
    assertBoundaryError,
  );
});
