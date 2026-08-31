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

const privateJwk = {
  crv: "Ed25519",
  d: "n59sESLoewoFFOeFu69aZXLI8YEOz9yXpkzOMFQnDp4",
  x: "SKaA_nE69844nq00przjwmcPcg5iRY1hL4fnfgipY94",
  kty: "OKP",
};
const privateKey = createPrivateKey({ key: privateJwk, format: "jwk" });
const installation = {
  installation_id: "test-installation",
  execution_realm_id: "test-realm",
  protocol_revision: "pillbox.huddles/1",
};
const runtimePolicy = {
  revision: "runtime/1",
  tool_policy: "deny_all",
  credential_bindings: [],
  egress: "credential_hosts_only",
};
const execution = {
  placement: "managed_container",
  requested: { provider: "opencode", model: "managed-model" },
  transport: {
    harness: "opencode",
    transport: "cloudflare-service-binding",
    harness_version: "opencode/managed",
    adapter_revision: "pillbox/huddles-invoke-v1",
  },
  context_renderer_revision: "huddles/context/1",
};
const outputFormat = {
  type: "json_schema",
  schema: { kind: "document", text: "string" },
  retry_count: 2,
};
const executionPolicyRevision = "execution/1";
const renderedInput = "Return one managed boundary probe.";
const renderedInputHash = digest(renderedInput);
const executionIdentityHash = digest({
  execution,
  execution_policy_revision: executionPolicyRevision,
});
const outputContractHash = digest(outputFormat);

test("managed boundary authorizes fresh retries before replay and carries signer identity", async () => {
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
      {
        env: {
          ...process.env,
          WRANGLER_LOG_PATH: join(persistence, "wrangler.log"),
        },
      },
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

    const workspaceId = "managed-workspace";
    const effectId = "managed-effect";
    const invocationId = "managed-invocation";
    const packetId = "managed-packet";
    const runId = "managed-run";
    const principalId = "managed-principal";
    const policyId = "managed-policy";
    const canonicalRequest = {
      requested_model: "opencode/managed-model",
      run_id: runId,
      packet_id: packetId,
      activity_principal_id: principalId,
      policy_id: policyId,
      execution,
    };
    const ensureBinding = requestBinding({
      sessionIdempotencyKey: effectId,
      deliveryReceiptId: "managed-delivery-1",
    });
    const ensureGrant = signedExecutionGrant({
      grantId: "managed-grant-ensure",
      sessionIdempotencyKey: effectId,
      deliveryReceiptId: "managed-delivery-1",
    });
    const ensured = await callPrivate(caller, "/ensure", {
      workspace_id: workspaceId,
      effect_id: effectId,
      canonical_request: canonicalRequest,
      managed_authorization: {
        grant: ensureGrant,
        request_binding: ensureBinding,
      },
    });
    assert.equal(ensured.disposition, "reused");

    const invokeBinding = requestBinding({
      sessionIdempotencyKey: invocationId,
      deliveryReceiptId: "managed-delivery-1",
    });
    const invokeBase = {
      workspace_id: workspaceId,
      effect_id: effectId,
      invocation_id: invocationId,
      activity_principal_id: principalId,
      policy_id: policyId,
      run_id: runId,
      packet_id: packetId,
      session_ref: ensured.session_ref,
      delivery_receipt_id: "managed-delivery-1",
      rendered_input: renderedInput,
      rendered_input_hash: renderedInputHash,
      tool_policy: "deny_all",
      requested_model: "opencode/managed-model",
      execution,
      execution_policy_revision: executionPolicyRevision,
      output_format: outputFormat,
    };
    const { activity_principal_id: _omittedPrincipal, ...missingPrincipal } =
      invokeBase;
    await assert.rejects(
      callPrivate(caller, "/invoke", {
        ...missingPrincipal,
        managed_authorization: {
          grant: signedExecutionGrant({
            grantId: "managed-grant-missing-principal",
            sessionIdempotencyKey: invocationId,
            deliveryReceiptId: "managed-delivery-1",
          }),
          request_binding: invokeBinding,
        },
      }),
      (error) => {
        assert.match(error.message, /activity_principal_id/);
        return true;
      },
    );
    const firstInvoke = await callPrivate(caller, "/invoke", {
      ...invokeBase,
      managed_authorization: {
        grant: signedExecutionGrant({
          grantId: "managed-grant-invoke-1",
          sessionIdempotencyKey: invocationId,
          deliveryReceiptId: "managed-delivery-1",
          expiresIn: 1,
        }),
        request_binding: invokeBinding,
      },
    });
    assert.equal(firstInvoke.status, "failed");
    assert.equal(firstInvoke.disposition, "created");
    assert.equal(firstInvoke.error.code, "runtime_unavailable");

    // Model a response-loss retry: only the signed authorization envelope
    // changes, after the original grant has expired. Pillbox must currentness-
    // check this fresh grant before it reuses the bounded D1 terminal result.
    await new Promise((resolve) => setTimeout(resolve, 2_100));
    const secondInvoke = await callPrivate(caller, "/invoke", {
      ...invokeBase,
      managed_authorization: {
        grant: signedExecutionGrant({
          grantId: "managed-grant-invoke-2",
          sessionIdempotencyKey: invocationId,
          deliveryReceiptId: "managed-delivery-1",
          expiresIn: 120,
        }),
        request_binding: invokeBinding,
      },
    });
    assert.deepEqual(secondInvoke, { ...firstInvoke, disposition: "reused" });

    const conflictRequest = {
      ...invokeBase,
      delivery_receipt_id: "managed-delivery-2",
      managed_authorization: {
        grant: signedExecutionGrant({
          grantId: "managed-grant-conflict",
          sessionIdempotencyKey: invocationId,
          deliveryReceiptId: "managed-delivery-2",
          expiresIn: 120,
        }),
        request_binding: requestBinding({
          sessionIdempotencyKey: invocationId,
          deliveryReceiptId: "managed-delivery-2",
        }),
      },
    };
    await assert.rejects(
      callPrivate(caller, "/invoke", conflictRequest),
      (error) => {
        assert.equal(error.code, "invoke_session_conflict");
        assert.match(error.existing_request_hash, /^sha256:[0-9a-f]{64}$/);
        assert.match(error.requested_request_hash, /^sha256:[0-9a-f]{64}$/);
        assert.notEqual(error.existing_request_hash, error.requested_request_hash);
        return true;
      },
    );

    const changedPrincipal = "managed-principal-2";
    await assert.rejects(
      callPrivate(caller, "/invoke", {
        ...invokeBase,
        activity_principal_id: changedPrincipal,
        managed_authorization: {
          grant: signedExecutionGrant({
            grantId: "managed-grant-principal-conflict",
            sessionIdempotencyKey: invocationId,
            deliveryReceiptId: "managed-delivery-1",
            principalId: changedPrincipal,
          }),
          request_binding: requestBinding({
            sessionIdempotencyKey: invocationId,
            deliveryReceiptId: "managed-delivery-1",
            principalId: changedPrincipal,
          }),
        },
      }),
      (error) => {
        assert.equal(error.code, "invoke_session_conflict");
        assert.notEqual(error.existing_request_hash, error.requested_request_hash);
        return true;
      },
    );

    const callsResponse = await authority.fetch("http://authority.test/calls");
    const calls = await callsResponse.json();
    assert.equal(
      calls.length,
      5,
      "ensure, first invoke, fresh retry, delivery conflict, principal conflict",
    );
    assert.ok(
      calls.every(
        (call) =>
          call.version === "pillbox.authorization-currentness/2" &&
          call.verified_signer.algorithm === "Ed25519" &&
          call.verified_signer.key_id === "test-key" &&
          call.verified_signer.public_key_sha256 ===
            "sha256:be7c33f790cd7e862fbafca20d617cc3dd30c4d5785921a788124cebd7ffdf6b",
      ),
    );
    assert.notEqual(calls[1].grant.grant_id, calls[2].grant.grant_id);
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

function requestBinding({
  sessionIdempotencyKey,
  deliveryReceiptId,
  principalId = "managed-principal",
}) {
  return {
    principal_id: principalId,
    policy_id: "managed-policy",
    run_id: "managed-run",
    invocation_id: "managed-invocation",
    packet_id: "managed-packet",
    delivery_receipt_id: deliveryReceiptId,
    session_idempotency_key: sessionIdempotencyKey,
    rendered_input_hash: renderedInputHash,
    execution_policy_revision: executionPolicyRevision,
    output_format: outputFormat,
    runtime_policy: runtimePolicy,
  };
}

function signedExecutionGrant({
  grantId,
  sessionIdempotencyKey,
  deliveryReceiptId,
  expiresIn = 120,
  principalId = "managed-principal",
}) {
  const now = Math.floor(Date.now() / 1000);
  const claims = {
    version: "huddles.execution-grant/1",
    grant_id: grantId,
    installation,
    organization_id: "test-organization",
    workspace_id: "managed-workspace",
    policy: {
      principal_id: principalId,
      policy_id: "managed-policy",
    },
    operations: ["ensure_session", "invoke_session"],
    run_id: "managed-run",
    invocation_id: "managed-invocation",
    packet_id: "managed-packet",
    delivery_receipt_id: deliveryReceiptId,
    session_idempotency_key: sessionIdempotencyKey,
    rendered_input_hash: renderedInputHash,
    execution_identity_hash: executionIdentityHash,
    output_contract_hash: outputContractHash,
    runtime_policy: runtimePolicy,
    issued_at: now - 10,
    not_before: now - 10,
    expires_at: now + expiresIn,
  };
  return signedEnvelope(claims);
}

function signedEnvelope(claims) {
  return {
    algorithm: "Ed25519",
    key_id: "test-key",
    claims,
    signature: sign(
      null,
      Buffer.from(canonicalJson(claims)),
      privateKey,
    ).toString("base64url"),
  };
}

function digest(value) {
  const material = typeof value === "string" ? value : canonicalJson(value);
  return `sha256:${createHash("sha256").update(material).digest("hex")}`;
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}
