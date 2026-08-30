import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { registerHooks } from "node:module";
import { test } from "node:test";
import { computeRenderedInputHash } from "./src/codex_execution.ts";

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

const { legacyExecutionRequest } = await import(
  "./src/legacy_huddles_adapter.ts"
);

test("private entrypoint exposes generic execution lifecycle methods", async () => {
  const source = await readFile(new URL("./src/huddles_runtime.ts", import.meta.url), "utf8");
  assert.match(source, /async executeInvocation\(/);
  assert.match(source, /async getExecutionStatus\(/);
  assert.match(source, /async cancelInvocation\(/);
});

test("legacy Huddles invocation translates only execution-owned fields", async () => {
  const rendered_input = "Return structured output.";
  const translated = legacyExecutionRequest({
    workspace_id: "workspace-not-forwarded",
    effect_id: "effect-not-forwarded",
    invocation_id: "invocation-1",
    session_ref: { session_id: "session-1" },
    delivery_receipt_id: "delivery-1",
    rendered_input,
    rendered_input_hash: await computeRenderedInputHash(rendered_input),
    tool_policy: "deny_all",
    harness: "opencode",
    requested_model: "openai/gpt-5.6-sol",
    output_format: {
      type: "json_schema",
      schema: { type: "object" },
      retry_count: 2,
    },
  });

  assert.equal(translated.contract_version, "pillbox.execution/2");
  assert.equal(translated.idempotency_key, "delivery-1");
  assert.equal(translated.execution.transport.harness, "opencode");
  assert.equal(translated.execution.transport.transport, "http");
  assert.equal(translated.execution.requested.provider, "openai");
  assert.equal(translated.execution.requested.model, "gpt-5.6-sol");
  assert.doesNotMatch(JSON.stringify(translated), /workspace-not-forwarded|effect-not-forwarded/);
});
