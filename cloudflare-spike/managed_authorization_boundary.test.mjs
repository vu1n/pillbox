import assert from "node:assert/strict";
import { createHash, createPrivateKey, sign } from "node:crypto";
import { execFile } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";
import { unstable_dev } from "wrangler";

const execFileAsync = promisify(execFile);
const privateKey = createPrivateKey({
  key: {
    crv: "Ed25519",
    d: "n59sESLoewoFFOeFu69aZXLI8YEOz9yXpkzOMFQnDp4",
    x: "SKaA_nE69844nq00przjwmcPcg5iRY1hL4fnfgipY94",
    kty: "OKP",
  },
  format: "jwk",
});
const installation = {
  installation_id: "test-installation",
  execution_realm_id: "test-realm",
  protocol_revision: "pillbox.huddles/1",
};

test("managed runtime is v2-only and authorizes every lifecycle operation", async () => {
  const persistence = await mkdtemp(join(tmpdir(), "pillbox-managed-auth-"));
  let authority;
  let target;
  let caller;
  try {
    const workerOptions = {
      logLevel: "none",
      experimental: { fileBasedRegistry: true, enableIpc: true },
    };
    authority = await unstable_dev("test/managed_auth_worker.ts", {
      ...workerOptions,
      config: "wrangler.managed-auth-authority.toml",
      persist: false,
    });
    await execFileAsync(
      "npx",
      [
        "wrangler",
        "d1",
        "migrations",
        "apply",
        "pillbox-managed-auth-test",
        "--local",
        "--persist-to",
        persistence,
        "--config",
        "wrangler.managed-auth-test.toml",
      ],
      { env: { ...process.env, WRANGLER_LOG_PATH: join(persistence, "wrangler.log") } },
    );
    await execFileAsync(
      "npx",
      [
        "wrangler",
        "d1",
        "execute",
        "pillbox-managed-auth-test",
        "--local",
        "--persist-to",
        persistence,
        "--config",
        "wrangler.managed-auth-test.toml",
        "--command",
        "INSERT INTO managed_execution_allowance (singleton, deployment_epoch, execution_limit, reserved_executions) VALUES (1, 'managed-auth-test-v1', 2, 0)",
      ],
      { env: { ...process.env, WRANGLER_LOG_PATH: join(persistence, "wrangler.log") } },
    );
    target = await unstable_dev("src/worker.ts", {
      ...workerOptions,
      config: "wrangler.managed-auth-test.toml",
      persistTo: persistence,
    });
    await target.fetch("http://pillbox-managed-auth.test/health");
    await new Promise((resolve) => setTimeout(resolve, 250));
    caller = await unstable_dev("test/ensure_worker.ts", {
      ...workerOptions,
      config: "wrangler.managed-auth-caller.toml",
      persist: false,
    });

    const legacyProbe = await caller.fetch("http://managed-auth.test/ensure", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    });
    assert.equal(legacyProbe.status, 404, "managed bridge has no legacy compatibility route");

    const executeRequest = {
      contract_version: "pillbox.execution/2",
      session_ref: { session_id: "generic-session" },
      invocation_id: "generic-invocation",
      idempotency_key: "generic-delivery",
      rendered_input: "Return one generic boundary probe.",
      rendered_input_hash: digest("Return one generic boundary probe."),
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
        context_renderer_revision: "huddles/context/1",
      },
      execution_policy_revision: "execution/2",
      output_format: { type: "text", retry_count: 0 },
    };
    const executeAuthorization = signedAuthorization(
      "execute",
      executeRequest,
      "generic-grant-execute",
    );

    await assert.rejects(
      callPrivate(caller, "/execute", { request: executeRequest }),
      /operation authorization is invalid/,
    );
    for (const [mismatch, authorization] of [
      ["operation_mismatch", signedAuthorization("cancel", executeRequest, "wrong-operation")],
      ["invocation_mismatch", signedAuthorization("execute", executeRequest, "wrong-invocation", { invocation_id: "other" })],
      ["request_digest_mismatch", signedAuthorization("execute", executeRequest, "wrong-digest", { request_digest: `sha256:${"0".repeat(64)}` })],
    ]) {
      await assert.rejects(
        callPrivate(caller, "/execute", { request: executeRequest, authorization }),
        new RegExp(`grant ${mismatch}`),
      );
    }
    await assert.rejects(
      callPrivate(caller, "/execute", {
        request: executeRequest,
        authorization: signedAuthorization("execute", executeRequest, "expired", {
          expires_at: Math.floor(Date.now() / 1000) - 1,
        }),
      }),
      /outside its validity interval/,
    );
    await assert.rejects(
      callPrivate(caller, "/execute", {
        request: executeRequest,
        authorization: signedAuthorization("execute", executeRequest, "wrong-signer", {}, "other-key"),
      }),
      /key is not trusted/,
    );
    await assert.rejects(
      callPrivate(caller, "/execute", {
        request: executeRequest,
        authorization: signedAuthorization("execute", executeRequest, "wrong-deployment", {
          installation: { ...installation, installation_id: "other-installation" },
        }),
      }),
      /installation does not match this deployment/,
    );
    await assert.rejects(
      callPrivate(caller, "/execute", {
        request: executeRequest,
        authorization: signedAuthorization("execute", executeRequest, "revoked"),
      }),
      /grant is not current/,
    );

    const first = await callPrivate(caller, "/execute", {
      request: executeRequest,
      authorization: executeAuthorization,
    });
    assert.equal(first.status, "failed");
    assert.equal(first.disposition, "created");
    assert.equal(first.error.code, "runtime_unavailable");
    const retry = await callPrivate(caller, "/execute", {
      request: executeRequest,
      authorization: executeAuthorization,
    });
    assert.deepEqual(retry, { ...first, disposition: "reused" });

    const statusRequest = {
      contract_version: "pillbox.execution/2",
      invocation_id: executeRequest.invocation_id,
      evidence_after: 0,
      evidence_limit: 100,
    };
    const status = await callPrivate(caller, "/status", {
      request: statusRequest,
      authorization: signedAuthorization("status", statusRequest, "grant-status"),
    });
    assert.equal(status.status, "failed");
    const cancelRequest = {
      contract_version: "pillbox.execution/2",
      invocation_id: executeRequest.invocation_id,
      idempotency_key: "generic-cancel",
      reason: "boundary probe complete",
    };
    const cancelled = await callPrivate(caller, "/cancel", {
      request: cancelRequest,
      authorization: signedAuthorization("cancel", cancelRequest, "grant-cancel"),
    });
    assert.equal(cancelled.status, "failed", "authorized cancel cannot rewrite a terminal result");

    const missingStatus = { ...statusRequest, invocation_id: "never-persisted" };
    await assert.rejects(
      callPrivate(caller, "/status", { request: missingStatus }),
      /operation authorization is invalid/,
    );
    await assert.rejects(
      callPrivate(caller, "/cancel", {
        request: { ...cancelRequest, invocation_id: "never-persisted" },
      }),
      /operation authorization is invalid/,
    );

    const calls = await (await authority.fetch("http://authority.test/calls")).json();
    assert.ok(calls.every((call) => call.version === "pillbox.authorization-currentness/3"));
    assert.deepEqual(
      calls.map((call) => call.expected.operation),
      ["execute", "execute", "execute", "status", "cancel"],
      "revoked check, create, exact retry, status, and cancel each recheck currentness",
    );
    assert.deepEqual(calls[1].grant, executeAuthorization.grant);
    assert.equal(JSON.stringify({ first, retry, status, cancelled }).includes("managed-principal"), false);
  } finally {
    if (caller) await caller.stop();
    if (target) await target.stop();
    if (authority) await authority.stop();
    await rm(persistence, { recursive: true, force: true });
  }
});

async function callPrivate(worker, path, input) {
  const response = await worker.fetch(`http://managed-auth.test${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input),
  });
  const body = await response.json();
  if (!response.ok) {
    const error = new Error(body.error?.message ?? "managed call failed");
    Object.assign(error, body.error);
    error.name = body.error?.name ?? error.name;
    throw error;
  }
  return body;
}

function signedAuthorization(operation, request, grantId, claimChanges = {}, keyId = "test-key") {
  const now = Math.floor(Date.now() / 1000);
  const claims = {
    version: "huddles.execution-operation-grant/2",
    grant_id: grantId,
    installation,
    organization_id: "test-organization",
    workspace_id: "managed-workspace",
    principal_id: "managed-principal",
    policy_id: "managed-policy",
    operation,
    invocation_id: request.invocation_id,
    request_digest: digest(request),
    issued_at: now - 10,
    not_before: now - 10,
    expires_at: now + 120,
    ...claimChanges,
  };
  return {
    grant: {
      algorithm: "Ed25519",
      key_id: keyId,
      claims,
      signature: sign(null, Buffer.from(canonicalJson(claims)), privateKey).toString("base64url"),
    },
  };
}

function digest(value) {
  const material = typeof value === "string" ? value : canonicalJson(value);
  return `sha256:${createHash("sha256").update(material).digest("hex")}`;
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.entries(value)
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([key, member]) => `${JSON.stringify(key)}:${canonicalJson(member)}`)
    .join(",")}}`;
}
