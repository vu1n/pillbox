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

test("managed entrypoint is v2-only and local legacy is a separate non-production export", async () => {
  const source = await readFile(new URL("./src/huddles_runtime.ts", import.meta.url), "utf8");
  const worker = await readFile(new URL("./src/worker.ts", import.meta.url), "utf8");
  const managedStart = source.indexOf("export class HuddlesRuntimeEntrypoint");
  const localStart = source.indexOf("export class LocalLegacyRuntimeEntrypoint");
  const managed = source.slice(managedStart, localStart);
  const local = source.slice(localStart, source.indexOf("export function executionService"));

  assert.ok(managedStart >= 0 && localStart > managedStart);
  assert.match(managed, /async executeInvocation\(/);
  assert.match(managed, /async getExecutionStatus\(/);
  assert.match(managed, /async cancelInvocation\(/);
  assert.doesNotMatch(managed, /ensureSession|invokeSession/);
  assert.match(local, /async ensureSession\(/);
  assert.match(local, /async invokeSession\(/);
  assert.doesNotMatch(local, /authorizeExecutionOperation|PillboxAuthorizationCurrentness/);
  assert.doesNotMatch(worker, /LocalLegacyRuntimeEntrypoint/);
  assert.match(
    source,
    /admission: managedAdmissionPolicy\(env\.MANAGED_EXECUTION_ENABLED\)/,
  );
});

test("public provisioning and local legacy invocation keep the admission guard", async () => {
  const worker = await readFile(new URL("./src/worker.ts", import.meta.url), "utf8");
  const runtime = await readFile(
    new URL("./src/huddles_runtime.ts", import.meta.url),
    "utf8",
  );

  const provisionGuard = worker.indexOf(
    'new URL(req.url).pathname === "/v2/workspaces/provision"',
  );
  assert.ok(provisionGuard >= 0);
  assert.ok(worker.indexOf("requireManagedAdmission", provisionGuard) >= provisionGuard);
  assert.ok(
    worker.indexOf("routeWorkspaceTransfer(req, env)", provisionGuard) > provisionGuard,
  );

  const invoke = runtime.indexOf("async invokeSession(", runtime.indexOf("LocalLegacyRuntimeEntrypoint"));
  const admission = runtime.indexOf("requireManagedAdmission", invoke);
  const execution = runtime.indexOf("executionService(this.env)", invoke);
  assert.ok(invoke >= 0 && admission > invoke && execution > admission);
});

test("private Huddles execution validates burn-in bootstrap before service construction", async () => {
  const runtime = await readFile(
    new URL("./src/huddles_runtime.ts", import.meta.url),
    "utf8",
  );
  const factory = runtime.indexOf("export function executionService(");
  const bootstrap = runtime.indexOf("requireManagedBurninBootstrap(env)", factory);
  const store = runtime.indexOf("new D1ExecutionStore", factory);

  assert.ok(factory >= 0);
  assert.ok(bootstrap > factory);
  assert.ok(store > bootstrap);
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
  assert.equal(translated.idempotency_key, "invocation-1");
  assert.equal(translated.execution.transport.harness, "opencode");
  assert.equal(translated.execution.transport.transport, "http");
  assert.equal(translated.execution.requested.provider, "openai");
  assert.equal(translated.execution.requested.model, "gpt-5.6-sol");
  assert.doesNotMatch(JSON.stringify(translated), /workspace-not-forwarded|effect-not-forwarded/);
});
