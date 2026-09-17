import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import test from "node:test";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "@cloudflare/sandbox") {
      return {
        shortCircuit: true,
        url: "data:text/javascript,export const getSandbox = () => { throw new Error('test must inject sandboxFor'); }; export class Sandbox {};",
      };
    }
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

const {
  MAX_R2_CAPTURE_STDOUT_BYTES,
  parseR2Capture,
  parseR2CaptureOutput,
} = await import("./src/r2_capture.ts");
const { routeWorkspaceTransfer } = await import("./src/workspace_transfer.ts");
const { signManagedCapability } = await import("./src/auth.ts");

const capabilitySecret = "workspace-transfer-capture-test-secret";
const repo = {
  endpoint: "https://account.r2.cloudflarestorage.com",
  region: "auto",
  bucket: "workspaces",
  prefix: "/project/run/",
  access_key: "scoped-ak",
  secret_key: "scoped-sk",
  session_token: "scoped-session",
};
const owner = {
  domain: "huddles_workspace",
  digest: `sha256:${"a".repeat(64)}`,
};

function expected(operation = "snapshot_restore") {
  return { operation, bucket: repo.bucket, prefix: repo.prefix };
}

function captureDocument(operation = "snapshot_restore", record = undefined) {
  const captureId = "00000000-0000-4000-8000-000000000001";
  return {
    schema_version: 1,
    capture_type: "r2_http_operation",
    capture_id: captureId,
    operation,
    bucket: repo.bucket,
    prefix: "project/run/",
    started_at: "2026-09-17T00:00:00.123456789Z",
    finished_at: "2026-09-17T00:00:01.123456789Z",
    status: "complete",
    reasons: [],
    records: [
      record ?? {
        id: `${captureId}:1`,
        method: "GET",
        action: "GetObject",
        selector: "project/run/objects/one",
        status: 200,
        request_body_bytes: 0,
        response_body_bytes: 42,
        response_complete: true,
      },
    ],
  };
}

function helperOutput(operation, status = "completed") {
  const capture = captureDocument(operation);
  if (status === "failed") {
    capture.status = "incomplete";
    capture.reasons = ["operation_failed"];
    capture.records[0].status = 500;
  }
  return JSON.stringify({
    schema_version: 1,
    status,
    snapshot: status === "completed" && operation === "snapshot_finalize" ? "f".repeat(64) : null,
    capture,
  });
}

function provisionBody(sessionId = "capture-session") {
  return {
    sessionId,
    invocationId: `${sessionId}-invocation`,
    executionRequestHash: `sha256:${"c".repeat(64)}`,
    workspace: {
      repo,
      password: "repo-password",
      snapshot: "e".repeat(64),
    },
  };
}

function finalizeBody(sessionId = "capture-session") {
  return {
    sessionId,
    workspace: {
      repo,
      password: "repo-password",
      snapshot: "e".repeat(64),
    },
  };
}

async function signedRequest(path, body, operation) {
  const encoded = JSON.stringify(body);
  const requestHash = `sha256:${[
    ...new Uint8Array(
      await crypto.subtle.digest("SHA-256", new TextEncoder().encode(encoded)),
    ),
  ].map((byte) => byte.toString(16).padStart(2, "0")).join("")}`;
  const token = await signManagedCapability({
    version: 1,
    subject: "controller:capture-test",
    audience: "pillbox-managed",
    expires_at_ms: Date.now() + 60_000,
    operation,
    request_sha256: requestHash,
    session_id: body.sessionId,
    ...(body.invocationId === undefined ? {} : { invocation_id: body.invocationId }),
  }, capabilitySecret);
  return new Request(`https://pillbox.test${path}`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}` },
    body: encoded,
  });
}

function transferEnv(capture = false) {
  return {
    EXECUTION_DB: {},
    MANAGED_CAPABILITY_SECRET: capabilitySecret,
    MANAGED_EXECUTION_EPOCH: "capture-test",
    MANAGED_EXECUTION_LIMIT: "2",
    ...(capture ? { MANAGED_R2_CAPTURE_ENABLED: "1" } : {}),
  };
}

class MemoryReservations {
  record = null;

  async claimProvision(input, allowance) {
    if (this.record !== null) {
      const same = this.record.invocation_id === input.invocation_id &&
        this.record.execution_request_hash === input.execution_request_hash &&
        this.record.owner.digest === input.owner.digest;
      return { kind: same ? "reused" : "conflict", record: this.record };
    }
    this.record = {
      invocation_id: input.invocation_id,
      session_id: input.session_id,
      owner: input.owner,
      execution_request_hash: input.execution_request_hash,
      source: "workspace_provision",
      provision_request_digest: input.provision_request_digest,
      status: "provisioning",
      error_code: null,
      allowance_epoch: allowance.deployment_epoch,
      allowance_limit: allowance.execution_limit,
      created_at_ms: input.now_ms,
      updated_at_ms: input.now_ms,
    };
    return { kind: "created", record: this.record };
  }

  async claimExecution() {
    throw new Error("unused");
  }

  async getReady() {
    return null;
  }

  async getSessionOwner() {
    return owner;
  }

  async markReady() {
    this.record = this.record === null ? null : { ...this.record, status: "ready" };
    return true;
  }

  async markFailed(input) {
    this.record = this.record === null
      ? null
      : { ...this.record, status: "failed", error_code: input.error_code };
    return true;
  }

  async getAllowance(allowance) {
    return { ...allowance, reserved_executions: this.record === null ? 0 : 1 };
  }
}

class MemoryFinalizeStore {
  record = null;

  async claim(input) {
    if (this.record === null) {
      this.record = {
        ...input,
        status: "running",
        result_snapshot: null,
        error_code: null,
        error_message: null,
        created_at_ms: input.now_ms,
        updated_at_ms: input.now_ms,
      };
      return { kind: "created", record: this.record };
    }
    return this.record.finalize_id === input.finalize_id &&
      this.record.request_digest === input.request_digest
      ? { kind: "reused", record: this.record }
      : { kind: "conflict", record: this.record };
  }

  async get() {
    return this.record;
  }

  async complete(input) {
    if (this.record?.finalize_id !== input.finalize_id || this.record.status !== "running") return false;
    this.record = { ...this.record, status: "completed", result_snapshot: input.result_snapshot, updated_at_ms: input.now_ms };
    return true;
  }

  async fail(input) {
    if (this.record?.finalize_id !== input.finalize_id || this.record.status !== "running") return false;
    this.record = { ...this.record, status: "failed", error_code: input.error_code, error_message: input.error_message, updated_at_ms: input.now_ms };
    return true;
  }
}

test("capture-v1 accepts scoped complete records and safe unknown operations", () => {
  const complete = parseR2Capture(captureDocument(), expected());
  assert.equal(complete?.records.length, 1);
  assert.equal(complete?.records[0].selector, "project/run/objects/one");

  const unknown = captureDocument("verification", {
    id: "00000000-0000-4000-8000-000000000001:1",
    method: "OTHER",
    action: "unknown",
    selector: null,
    status: 200,
    request_body_bytes: 0,
    response_body_bytes: 0,
    response_complete: true,
  });
  unknown.status = "incomplete";
  unknown.reasons = ["foreign_selector"];
  const parsed = parseR2Capture(unknown, expected("verification"));
  assert.equal(parsed?.records[0].action, "unknown");
  assert.equal(parsed?.records[0].selector, null);
});

test("capture-v1 rejects malformed, foreign, and overflowing records", () => {
  const malformed = { ...captureDocument(), extra: "not allowed" };
  assert.equal(parseR2Capture(malformed, expected()), null);

  const foreignBucket = { ...captureDocument(), bucket: "other-bucket" };
  assert.equal(parseR2Capture(foreignBucket, expected()), null);

  const foreignSelector = captureDocument();
  foreignSelector.records[0].selector = "other/secret";
  assert.equal(parseR2Capture(foreignSelector, expected()), null);

  const overflow = captureDocument();
  overflow.status = "incomplete";
  overflow.reasons = ["record_limit"];
  overflow.records = Array.from({ length: 257 }, (_, index) => ({
    id: `${overflow.capture_id}:${index + 1}`,
    method: "GET",
    action: "GetObject",
    selector: `project/run/object-${index}`,
    status: 200,
    request_body_bytes: 0,
    response_body_bytes: 0,
    response_complete: true,
  }));
  assert.equal(parseR2Capture(overflow, expected()), null);

  const oversizedString = captureDocument();
  oversizedString.records[0].selector = "x".repeat(1_100_000);
  assert.equal(parseR2Capture(oversizedString, expected()), null);

  const oversizedEnvelope = JSON.stringify({
    schema_version: 1,
    status: "completed",
    snapshot: null,
    capture: "x".repeat(1_100_000),
  });
  assert.deepEqual(parseR2CaptureOutput(oversizedEnvelope, expected()), {
    status: "unavailable",
    reason: "helper_response_oversized",
  });
});

test("helper parser bounds stdout and never returns malformed capture data", () => {
  assert.deepEqual(
    parseR2CaptureOutput("{not-json", expected()),
    { status: "unavailable", reason: "helper_response_malformed" },
  );
  assert.deepEqual(
    parseR2CaptureOutput("x".repeat(MAX_R2_CAPTURE_STDOUT_BYTES + 1), expected()),
    { status: "unavailable", reason: "helper_response_oversized" },
  );
  const foreign = JSON.parse(helperOutput("snapshot_restore"));
  foreign.capture.bucket = "foreign-bucket";
  assert.deepEqual(parseR2CaptureOutput(JSON.stringify(foreign), expected()), {
    status: "completed",
    snapshot: null,
    capture: { status: "unavailable", reason: "capture_invalid" },
  });
  const contradictoryFailure = JSON.parse(helperOutput("snapshot_restore"));
  contradictoryFailure.status = "failed";
  assert.deepEqual(
    parseR2CaptureOutput(JSON.stringify(contradictoryFailure), expected()),
    {
      status: "failed",
      snapshot: null,
      capture: { status: "unavailable", reason: "capture_invalid" },
    },
  );
});

test("provision capture is opt-in, passes the helper flag, and replay is unavailable", async () => {
  const reservations = new MemoryReservations();
  const commands = [];
  let executions = 0;
  const dependencies = {
    reservationStore: reservations,
    sandboxFor: async () => ({
      killAllProcesses: async () => {},
      exec: async (command) => {
        executions += 1;
        commands.push(command);
        return { success: true, stdout: helperOutput("snapshot_restore"), stderr: "" };
      },
    }),
    now: () => 1,
  };
  const body = provisionBody();
  const response = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/provision", body, "workspace_provision"),
    transferEnv(true),
    dependencies,
  );
  assert.equal(response?.status, 200);
  const first = await response.json();
  assert.equal(first.r2Capture.capture_type, "r2_http_operation");
  assert.match(commands[0], /--capture-r2-json/);

  const replay = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/provision", body, "workspace_provision"),
    transferEnv(true),
    dependencies,
  );
  assert.equal(replay?.status, 200);
  assert.deepEqual((await replay.json()).r2Capture, { status: "unavailable", reason: "replayed" });
  assert.equal(executions, 1);
});

test("restore keeps a validated helper completion when capture telemetry is missing", async () => {
  const reservations = new MemoryReservations();
  const dependencies = {
    reservationStore: reservations,
    sandboxFor: async () => ({
      killAllProcesses: async () => {},
      exec: async () => {
        const output = JSON.parse(helperOutput("snapshot_restore"));
        delete output.capture;
        return { success: true, stdout: JSON.stringify(output), stderr: "" };
      },
    }),
    now: () => 1,
  };
  const response = await routeWorkspaceTransfer(
    await signedRequest(
      "/v2/workspaces/provision",
      provisionBody("restore-missing-capture"),
      "workspace_provision",
    ),
    transferEnv(true),
    dependencies,
  );
  assert.equal(response?.status, 200);
  const body = await response.json();
  assert.deepEqual(body.r2Capture, { status: "unavailable", reason: "capture_invalid" });
  assert.equal(reservations.record.status, "ready");
});

test("finalize kills processes before the credentialed capture helper and replay stays unavailable", async () => {
  const events = [];
  const reservations = new MemoryReservations();
  const finalizeStore = new MemoryFinalizeStore();
  const dependencies = {
    reservationStore: reservations,
    finalizeStore,
    sandboxFor: async () => ({
      killAllProcesses: async () => events.push("kill"),
      exec: async (command, options) => {
        events.push(["exec", command, options.env.PILLBOX_R2_ACCESS_KEY]);
        return { success: true, stdout: helperOutput("snapshot_finalize"), stderr: "" };
      },
    }),
    now: () => 1,
  };
  const body = finalizeBody();
  const response = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/finalize", body, "workspace_finalize"),
    transferEnv(true),
    dependencies,
  );
  assert.equal(response?.status, 200);
  const first = await response.json();
  assert.equal(first.resultSnapshot, "f".repeat(64));
  assert.equal(first.r2Capture.capture_type, "r2_http_operation");
  assert.equal(events[0], "kill");
  assert.equal(events[1][0], "exec");
  assert.match(events[1][1], /--capture-r2-json/);
  assert.equal(events[1][2], "scoped-ak");

  const replay = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/finalize", body, "workspace_finalize"),
    transferEnv(true),
    dependencies,
  );
  assert.equal(replay?.status, 200);
  assert.deepEqual((await replay.json()).r2Capture, { status: "unavailable", reason: "replayed" });
  assert.equal(events.filter((event) => event === "kill").length, 1);
});

test("capture helper failures return only validated capture and no stderr/stdout", async () => {
  const reservations = new MemoryReservations();
  const finalizeStore = new MemoryFinalizeStore();
  const secret = "LEAKED-HELPER-STDERR";
  const dependencies = {
    reservationStore: reservations,
    finalizeStore,
    sandboxFor: async () => ({
      killAllProcesses: async () => {},
      exec: async () => ({
        success: false,
        stdout: helperOutput("snapshot_finalize", "failed"),
        stderr: secret,
      }),
    }),
    now: () => 1,
  };
  const response = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/finalize", finalizeBody("failure-session"), "workspace_finalize"),
    transferEnv(true),
    dependencies,
  );
  assert.equal(response?.status, 502);
  const body = await response.json();
  assert.equal(body.r2Capture.capture_type, "r2_http_operation");
  assert.equal(typeof body.finalizeId, "string");
  assert.equal(typeof body.requestDigest, "string");
  assert.doesNotMatch(JSON.stringify(body), new RegExp(secret));
});

test("finalize keeps a validated snapshot when capture telemetry is foreign", async () => {
  const reservations = new MemoryReservations();
  const finalizeStore = new MemoryFinalizeStore();
  const dependencies = {
    reservationStore: reservations,
    finalizeStore,
    sandboxFor: async () => ({
      killAllProcesses: async () => {},
      exec: async () => {
        const output = JSON.parse(helperOutput("snapshot_finalize"));
        output.capture.prefix = "other/foreign/";
        return { success: true, stdout: JSON.stringify(output), stderr: "" };
      },
    }),
    now: () => 1,
  };
  const body = finalizeBody("finalize-invalid-capture");
  const response = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/finalize", body, "workspace_finalize"),
    transferEnv(true),
    dependencies,
  );
  assert.equal(response?.status, 200);
  const result = await response.json();
  assert.equal(result.resultSnapshot, "f".repeat(64));
  assert.deepEqual(result.r2Capture, { status: "unavailable", reason: "capture_invalid" });
  assert.equal(finalizeStore.record.status, "completed");
  assert.equal(result.finalizeId, finalizeStore.record.finalize_id);
  assert.equal(result.requestDigest, finalizeStore.record.request_digest);
});

test("disabled capture preserves ordinary helper output and response shape", async () => {
  const reservations = new MemoryReservations();
  const commands = [];
  const dependencies = {
    reservationStore: reservations,
    sandboxFor: async () => ({
      killAllProcesses: async () => {},
      exec: async (command) => {
        commands.push(command);
        return { success: true, stdout: "ordinary restore output\n", stderr: "" };
      },
    }),
    now: () => 1,
  };
  const response = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/provision", provisionBody("ordinary-session"), "workspace_provision"),
    transferEnv(false),
    dependencies,
  );
  assert.equal(response?.status, 200);
  assert.deepEqual(await response.json(), { ok: true, disposition: "created" });
  assert.doesNotMatch(commands[0], /--capture-r2-json/);
});
