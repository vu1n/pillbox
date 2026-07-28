import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { unstable_dev } from "wrangler";

function request(canonicalRequest, workspaceId = "ws-huddles", effectId = "effect-42") {
  return { workspace_id: workspaceId, effect_id: effectId, canonical_request: canonicalRequest };
}

async function callEnsure(worker, input) {
  const response = await worker.fetch("http://ensure.test/ensure", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input),
  });
  const body = await response.json();
  if (!response.ok) {
    const error = new Error(body.error?.message ?? "ensure failed");
    error.code = body.error?.code;
    error.name = body.error?.name ?? error.name;
    throw error;
  }
  return body;
}

test("ensure is atomic, canonical, restart-safe, and private", async () => {
  const persistence = await mkdtemp(join(tmpdir(), "pillbox-ensure-"));
  let target;
  let caller;
  let restartedTarget;
  let restartedCaller;
  try {
    const workerOptions = {
      logLevel: "none",
      experimental: { fileBasedRegistry: true, enableIpc: true },
    };
    target = await unstable_dev("src/worker.ts", {
      ...workerOptions,
      config: "wrangler.toml",
      persistTo: persistence,
    });
    await target.fetch("http://pillbox.test/health");
    await new Promise((resolve) => setTimeout(resolve, 250));
    caller = await unstable_dev("test/ensure_worker.ts", {
      ...workerOptions,
      config: "wrangler.ensure-test.toml",
      persist: false,
    });

    const first = request({
      requested_model: "openai/gpt-5.6-sol",
      nested: { z: 1, a: 2 },
    });
    const concurrent = await Promise.all(
      Array.from({ length: 16 }, () => callEnsure(caller, first)),
    );

    assert.equal(concurrent.filter((result) => result.disposition === "created").length, 1);
    assert.equal(concurrent.filter((result) => result.disposition === "reused").length, 15);
    assert.ok(
      concurrent.every(
        (result) => result.session_ref.session_id === concurrent[0].session_ref.session_id,
      ),
    );
    for (const result of concurrent) {
      assert.deepEqual(result.attribution, {
        requested_model: "openai/gpt-5.6-sol",
        served_model: null,
        status: "unavailable",
      });
    }

    const reordered = request({
      nested: { a: 2, z: 1 },
      requested_model: "openai/gpt-5.6-sol",
    });
    assert.equal((await callEnsure(caller, reordered)).disposition, "reused");

    const otherWorkspace = await callEnsure(
      caller,
      request(first.canonical_request, "another-workspace", "effect-42"),
    );
    assert.equal(otherWorkspace.disposition, "created");
    assert.notEqual(otherWorkspace.session_ref.session_id, concurrent[0].session_ref.session_id);

    const formerlyAmbiguousLeft = await callEnsure(
      caller,
      request(first.canonical_request, "workspace\0suffix", "effect"),
    );
    const formerlyAmbiguousRight = await callEnsure(
      caller,
      request(first.canonical_request, "workspace", "suffix\0effect"),
    );
    assert.notEqual(
      formerlyAmbiguousLeft.session_ref.session_id,
      formerlyAmbiguousRight.session_ref.session_id,
    );

    await assert.rejects(
      callEnsure(
        caller,
        request({ requested_model: "openai/gpt-5.6-sol", nested: { a: 3, z: 1 } }),
      ),
      (error) => error?.code === "ensure_session_conflict",
    );

    await assert.rejects(
      callEnsure(
        caller,
        request({ nested: {} }),
      ),
      (error) => /requested_model/.test(String(error)),
    );

    const sessionId = concurrent[0].session_ref.session_id;
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
    await new Promise((resolve) => setTimeout(resolve, 250));
    restartedCaller = await unstable_dev("test/ensure_worker.ts", {
      ...workerOptions,
      config: "wrangler.ensure-test.toml",
      persist: false,
    });
    const afterRestart = await callEnsure(restartedCaller, first);
    assert.equal(afterRestart.disposition, "reused");
    assert.equal(afterRestart.session_ref.session_id, sessionId);

    const publicProbe = await restartedTarget.fetch(
      `http://${restartedTarget.address}/agents/session-gateway/${sessionId}/ensure`,
      { method: "POST" },
    );
    assert.equal(publicProbe.status, 404);
  } finally {
    if (restartedCaller) await restartedCaller.stop();
    if (restartedTarget) await restartedTarget.stop();
    if (caller) await caller.stop();
    if (target) await target.stop();
    await rm(persistence, { recursive: true, force: true });
  }
});
