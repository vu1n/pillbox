import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { unstable_dev } from "wrangler";

const execFileAsync = promisify(execFile);

function ensureRequest(canonicalRequest) {
  return {
    workspace_id: "ws-huddles",
    effect_id: "effect-42",
    canonical_request: canonicalRequest,
  };
}

async function callPrivate(worker, path, input) {
  const response = await worker.fetch(`http://ensure.test${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input),
  });
  const body = await response.json();
  if (!response.ok) {
    const error = new Error(body.error?.message ?? "private call failed");
    error.code = body.error?.code;
    throw error;
  }
  return body;
}

test("legacy Huddles RPC is a stateless adapter over bounded execution", async () => {
  const persistence = await mkdtemp(join(tmpdir(), "pillbox-execution-adapter-"));
  let target;
  let caller;
  let restartedTarget;
  let restartedCaller;
  const workerOptions = {
    logLevel: "none",
    experimental: { fileBasedRegistry: true, enableIpc: true },
  };
  try {
    await execFileAsync(
      "npx",
      [
        "wrangler",
        "d1",
        "migrations",
        "apply",
        "pillbox-execution-preview",
        "--local",
        "--persist-to",
        persistence,
        "--config",
        "wrangler.toml",
      ],
      { env: { ...process.env, WRANGLER_LOG_PATH: join(persistence, "wrangler.log") } },
    );
    target = await unstable_dev("src/worker.ts", {
      ...workerOptions,
      config: "wrangler.toml",
      persistTo: persistence,
    });
    await target.fetch("http://pillbox.test/health");
    caller = await unstable_dev("test/ensure_worker.ts", {
      ...workerOptions,
      config: "wrangler.ensure-test.toml",
      persist: false,
    });

    const canonical = {
      requested_model: "openai/gpt-5.6-sol",
      harness: "opencode",
      nested: { z: 1, a: 2 },
    };
    const ensured = await Promise.all(
      Array.from({ length: 16 }, () =>
        callPrivate(caller, "/ensure", ensureRequest(canonical)),
      ),
    );
    assert.ok(ensured.every((result) => result.disposition === "reused"));
    assert.ok(
      ensured.every(
        (result) =>
          result.session_ref.session_id === ensured[0].session_ref.session_id,
      ),
    );
    assert.deepEqual(ensured[0].attribution, {
      requested_model: "openai/gpt-5.6-sol",
      served_model: null,
      status: "unavailable",
    });

    const reordered = await callPrivate(
      caller,
      "/ensure",
      ensureRequest({
        nested: { a: 2, z: 1 },
        harness: "opencode",
        requested_model: "openai/gpt-5.6-sol",
      }),
    );
    assert.equal(reordered.session_ref.session_id, ensured[0].session_ref.session_id);
    await assert.rejects(
      callPrivate(caller, "/ensure", ensureRequest({ nested: {} })),
      /requested_model/,
    );

    const renderedInput = "Return one concise planning critique.";
    const invoke = {
      workspace_id: "ws-huddles",
      effect_id: "effect-42",
      invocation_id: "invocation-1",
      session_ref: ensured[0].session_ref,
      delivery_receipt_id: "delivery-1",
      rendered_input: renderedInput,
      rendered_input_hash: `sha256:${createHash("sha256").update(renderedInput).digest("hex")}`,
      tool_policy: "deny_all",
      harness: "opencode",
      requested_model: "openai/gpt-5.6-sol",
      output_format: {
        type: "json_schema",
        schema: { type: "object" },
        retry_count: 2,
      },
    };
    const unavailable = await callPrivate(caller, "/invoke", invoke);
    assert.deepEqual(unavailable, {
      status: "failed",
      disposition: "created",
      session_ref: ensured[0].session_ref,
      error: {
        code: "runtime_unavailable",
        message: "Pillbox managed runner has no Cloudflare Sandbox binding",
      },
    });
    assert.deepEqual(await callPrivate(caller, "/invoke", invoke), {
      ...unavailable,
      disposition: "reused",
    });
    await assert.rejects(
      callPrivate(caller, "/invoke", {
        ...invoke,
        delivery_receipt_id: "changed-delivery",
      }),
      (error) => error?.code === "invoke_session_conflict",
    );

    const unauthenticated = await target.fetch(
      `http://${target.address}/v2/executions/status`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          contract_version: "pillbox.execution/2",
          invocation_id: "invocation-1",
        }),
      },
    );
    assert.equal(unauthenticated.status, 401);

    await caller.stop();
    await target.stop();
    caller = undefined;
    target = undefined;

    restartedTarget = await unstable_dev("src/worker.ts", {
      ...workerOptions,
      config: "wrangler.toml",
      persistTo: persistence,
    });
    await restartedTarget.fetch("http://pillbox.test/health");
    restartedCaller = await unstable_dev("test/ensure_worker.ts", {
      ...workerOptions,
      config: "wrangler.ensure-test.toml",
      persist: false,
    });
    assert.deepEqual(await callPrivate(restartedCaller, "/invoke", invoke), {
      ...unavailable,
      disposition: "reused",
    });
  } finally {
    if (restartedCaller) await restartedCaller.stop();
    if (restartedTarget) await restartedTarget.stop();
    if (caller) await caller.stop();
    if (target) await target.stop();
    await rm(persistence, { recursive: true, force: true });
  }
});
