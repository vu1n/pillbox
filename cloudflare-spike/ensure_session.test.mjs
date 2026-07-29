import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { unstable_dev } from "wrangler";

function request(
  canonicalRequest,
  workspaceId = "ws-huddles",
  effectId = "effect-42",
) {
  return {
    workspace_id: workspaceId,
    effect_id: effectId,
    canonical_request: canonicalRequest,
  };
}

async function callEnsure(worker, input) {
  return callPrivate(worker, "/ensure", input);
}

async function callInvoke(worker, input) {
  return callPrivate(worker, "/invoke", input);
}

async function callPrivate(worker, path, input) {
  const response = await worker.fetch(`http://ensure.test${path}`, {
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

    const ordinaryPublicInput = await target.fetch(
      `http://${target.address}/agents/session-gateway/public-session/input`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "still public", target: "agent" }),
      },
    );
    assert.equal(
      ordinaryPublicInput.status,
      401,
      await ordinaryPublicInput.text(),
    );

    const reservedSessionId = `ensure-${createHash("sha256")
      .update(JSON.stringify(["ws-huddles", "effect-42"]))
      .digest("hex")}`;
    const reservedBeforeEnsure = await target.fetch(
      `http://${target.address}/agents/session-gateway/${reservedSessionId}/input`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "must never pre-bind", target: "agent" }),
      },
    );
    assert.equal(reservedBeforeEnsure.status, 404);

    const first = request({
      requested_model: "openai/gpt-5.6-sol",
      harness: "opencode",
      nested: { z: 1, a: 2 },
    });
    const concurrent = await Promise.all(
      Array.from({ length: 16 }, () => callEnsure(caller, first)),
    );

    assert.equal(
      concurrent.filter((result) => result.disposition === "created").length,
      1,
    );
    assert.equal(
      concurrent.filter((result) => result.disposition === "reused").length,
      15,
    );
    assert.ok(
      concurrent.every(
        (result) =>
          result.session_ref.session_id ===
          concurrent[0].session_ref.session_id,
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
      harness: "opencode",
      requested_model: "openai/gpt-5.6-sol",
    });
    assert.equal((await callEnsure(caller, reordered)).disposition, "reused");

    const otherWorkspace = await callEnsure(
      caller,
      request(first.canonical_request, "another-workspace", "effect-42"),
    );
    assert.equal(otherWorkspace.disposition, "created");
    assert.notEqual(
      otherWorkspace.session_ref.session_id,
      concurrent[0].session_ref.session_id,
    );

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
        request({
          requested_model: "openai/gpt-5.6-sol",
          harness: "opencode",
          nested: { a: 3, z: 1 },
        }),
      ),
      (error) => error?.code === "ensure_session_conflict",
    );

    await assert.rejects(callEnsure(caller, request({ nested: {} })), (error) =>
      /requested_model/.test(String(error)),
    );

    const sessionId = concurrent[0].session_ref.session_id;
    const publicInput = await target.fetch(
      `http://${target.address}/agents/session-gateway/${sessionId}/input`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ text: "must stay private", target: "agent" }),
      },
    );
    assert.equal(publicInput.status, 404);
    const publicSocket = new WebSocket(
      `ws://${target.address}/agents/session-gateway/${sessionId}/subscribe?from=1`,
    );
    const publicSocketOutcome = await new Promise((resolve, reject) => {
      const timeout = setTimeout(
        () => reject(new Error("managed session socket stayed open")),
        5_000,
      );
      publicSocket.addEventListener("open", () => {
        clearTimeout(timeout);
        resolve("opened");
      });
      publicSocket.addEventListener("close", (event) => {
        clearTimeout(timeout);
        resolve(`closed:${event.code}`);
      });
      publicSocket.addEventListener("error", () => {
        clearTimeout(timeout);
        resolve("rejected");
      });
    });
    assert.notEqual(publicSocketOutcome, "opened");
    const renderedInput = "Return one concise planning critique.";
    const invoke = {
      workspace_id: "ws-huddles",
      effect_id: "effect-42",
      invocation_id: "invocation-1",
      session_ref: { session_id: sessionId },
      delivery_receipt_id: "delivery-1",
      rendered_input: renderedInput,
      rendered_input_hash: `sha256:${createHash("sha256").update(renderedInput).digest("hex")}`,
      tool_policy: "deny_all",
      harness: "opencode",
      requested_model: "openai/gpt-5.6-sol",
    };
    const unavailable = await callInvoke(caller, invoke);
    assert.deepEqual(unavailable, {
      status: "failed",
      disposition: "created",
      session_ref: { session_id: sessionId, seq_range: [1, 2] },
      error: {
        code: "runtime_unavailable",
        message: "Pillbox managed runner has no Cloudflare Sandbox binding",
      },
    });
    assert.deepEqual(await callInvoke(caller, invoke), {
      ...unavailable,
      disposition: "reused",
    });
    await assert.rejects(
      callInvoke(caller, {
        ...invoke,
        delivery_receipt_id: "changed-delivery",
      }),
      (error) => error?.code === "invoke_session_conflict",
    );
    await assert.rejects(
      callInvoke(caller, {
        ...invoke,
        invocation_id: "invocation-bad-hash",
        rendered_input_hash: "sha256:wrong",
      }),
      (error) => /rendered_input_hash/.test(String(error)),
    );
    await assert.rejects(
      callInvoke(caller, {
        ...invoke,
        invocation_id: "invocation-bad-policy",
        tool_policy: "allow",
      }),
      (error) => /tool_policy/.test(String(error)),
    );

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
    assert.deepEqual(await callInvoke(restartedCaller, invoke), {
      ...unavailable,
      disposition: "reused",
    });

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
