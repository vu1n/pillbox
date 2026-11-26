import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";
import { unstable_dev } from "wrangler";
import { signManagedCapability } from "./src/auth.ts";

const execFileAsync = promisify(execFile);
const CONFIG = "wrangler.runtime-test.toml";
const SECRET = "public-route-test-secret-0123456789abcdef";

test("public execution route authorizes every lifecycle operation by capability", async () => {
  const persistence = await mkdtemp(join(tmpdir(), "pillbox-public-route-"));
  let worker;
  try {
    const wranglerEnv = {
      env: { ...process.env, WRANGLER_LOG_PATH: join(persistence, "wrangler.log") },
    };
    await execFileAsync(
      "npx",
      ["wrangler", "d1", "migrations", "apply", "pillbox-execution-preview", "--local",
        "--persist-to", persistence, "--config", CONFIG],
      wranglerEnv,
    );
    await execFileAsync(
      "npx",
      ["wrangler", "d1", "execute", "pillbox-execution-preview", "--local",
        "--persist-to", persistence, "--config", CONFIG, "--command",
        "INSERT INTO managed_execution_allowance (singleton, deployment_epoch, execution_limit, reserved_executions) VALUES (1, 'runtime-test-v1', 2, 0)"],
      wranglerEnv,
    );
    worker = await unstable_dev("src/worker.ts", {
      config: CONFIG,
      logLevel: "none",
      persistTo: persistence,
      vars: { MANAGED_CAPABILITY_SECRET: SECRET },
      experimental: { disableExperimentalWarning: true },
    });

    const executeRequest = {
      contract_version: "pillbox.execution/2",
      session_ref: { session_id: "public-session" },
      invocation_id: "public-invocation",
      idempotency_key: "public-invocation",
      rendered_input: "Return one public boundary probe.",
      rendered_input_hash: digest("Return one public boundary probe."),
      tool_policy: "deny_all",
      execution: {
        transport: {
          harness: "opencode",
          transport: "http",
          harness_version: "opencode/managed",
          adapter_revision: "pillbox/execution-v2",
        },
        requested: {
          provider: "zai-coding-plan",
          model: "managed-model",
          profile: null,
          reasoning_effort: "high",
        },
        placement: "managed_container",
        context_renderer_revision: "pillbox/context/1",
      },
      execution_policy_revision: "execution/2",
      output_format: { type: "text", retry_count: 0 },
    };

    const unauthenticated = await post(worker, "/v2/executions", executeRequest);
    assert.equal(unauthenticated.status, 401);
    assert.deepEqual(unauthenticated.body, { error: { code: "unauthenticated" } });

    const wrongOperation = await post(worker, "/v2/executions", executeRequest, "status");
    assert.equal(wrongOperation.status, 401, "a capability for another operation is rejected");

    const runtimeDefault = { ...executeRequest, tool_policy: "runtime_default" };
    const unsupported = await post(worker, "/v2/executions", runtimeDefault, "execute");
    assert.equal(unsupported.status, 400);
    assert.equal(unsupported.body.error.code, "unsupported_policy");

    const noncanonical = { ...executeRequest, idempotency_key: "public-delivery" };
    const rejected = await post(worker, "/v2/executions", noncanonical, "execute");
    assert.equal(rejected.status, 400);
    assert.match(rejected.body.error.message, /idempotency_key must equal invocation_id/);

    const first = await post(worker, "/v2/executions", executeRequest, "execute");
    assert.equal(first.status, 200);
    assert.equal(first.body.status, "failed");
    assert.equal(first.body.disposition, "created");
    assert.equal(first.body.error.code, "runtime_unavailable");
    const retry = await post(worker, "/v2/executions", executeRequest, "execute");
    assert.deepEqual(retry.body, { ...first.body, disposition: "reused" });

    const statusRequest = {
      contract_version: "pillbox.execution/2",
      invocation_id: executeRequest.invocation_id,
      evidence_after: 0,
      evidence_limit: 100,
    };
    const status = await post(worker, "/v2/executions/status", statusRequest, "status");
    assert.equal(status.status, 200);
    assert.equal(status.body.status, "failed");

    const otherOwner = await post(
      worker, "/v2/executions/status", statusRequest, "status", "controller:mallory",
    );
    assert.equal(otherOwner.status, 404, "another controller cannot read this execution");

    const cancelRequest = {
      contract_version: "pillbox.execution/2",
      invocation_id: executeRequest.invocation_id,
      idempotency_key: executeRequest.invocation_id,
      reason: "boundary probe complete",
    };
    const cancelled = await post(worker, "/v2/executions/cancel", cancelRequest, "cancel");
    assert.equal(cancelled.body.status, "failed", "cancel cannot rewrite a terminal result");
  } finally {
    if (worker) await worker.stop();
    await rm(persistence, { recursive: true, force: true });
  }
});

async function post(worker, path, request, operation, subject = "controller:public-route") {
  const body = JSON.stringify(request);
  const headers = { "content-type": "application/json" };
  if (operation !== undefined) {
    const token = await signManagedCapability(
      {
        version: 1,
        subject,
        audience: "pillbox-managed",
        expires_at_ms: Date.now() + 60_000,
        operation,
        request_sha256: digest(body),
        ...(operation === "execute" ? { session_id: request.session_ref.session_id } : {}),
        invocation_id: request.invocation_id,
      },
      SECRET,
    );
    headers.authorization = `Bearer ${token}`;
  }
  const response = await worker.fetch(`http://pillbox-public-route.test${path}`, {
    method: "POST",
    headers,
    body,
  });
  return { status: response.status, body: await response.json() };
}

function digest(value) {
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}
