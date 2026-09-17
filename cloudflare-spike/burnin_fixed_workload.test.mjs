import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { chmod, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createOperationSidecar } from "./scripts/operation-sidecar.mjs";

const script = new URL("./scripts/burnin-fixed-workload.mjs", import.meta.url);
const reconciler = new URL("./scripts/reconcile-burnin.mjs", import.meta.url);
const fixture = new URL("./testdata/burnin-reconciliation.fixture.json", import.meta.url);

function syntheticCapture(operation = "verification") {
  return {
    schema_version: 1, capture_type: "r2_http_operation",
    capture_id: "11111111-1111-4111-8111-111111111111", operation,
    bucket: "burnin", prefix: "snapshots/",
    started_at: "2026-09-17T00:00:00.000Z", finished_at: "2026-09-17T00:00:01.000Z",
    status: "complete", reasons: [], records: [{
      id: "11111111-1111-4111-8111-111111111111:1", method: "GET", action: "GetObject",
      selector: "snapshots/config", status: 200, request_body_bytes: 0,
      response_body_bytes: 42, response_complete: true,
    }],
  };
}

test("dry run distinguishes planned Analytics units from provider-observed writes", async () => {
  const result = await run(process.execPath, [script.pathname]);
  assert.equal(result.code, 0, result.stderr);
  const plan = JSON.parse(result.stdout);
  assert.deepEqual(plan.accounting, {
    analytics_points_planned: 1,
    analytics_points_observed: "provider capture required after the run",
    analytics_variance: "observed minus planned",
  });
});

test("operation capture rejects non-finalize and missing-output invocations", async () => {
  for (const [args, expected] of [
    [["--operation-capture-out", "unused.json"], /requires --finalize and an output path/],
    [["--finalize", "--operation-capture-out"], /requires a value/],
  ]) {
    const result = await run(process.execPath, [script.pathname, ...args]);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, expected);
  }
});

test("Pillbox brackets the sole Huddles recorder with preflight and same-report finalize", async (t) => {
  const temp = await mkdtemp(join(tmpdir(), "pillbox-burnin-preflight-"));
  t.after(() => rm(temp, { recursive: true, force: true }));
  const marker = join(temp, "preflight-ran");
  const preflight = join(temp, "managed-preflight.sh");
  const observedOutput = "pillbox: run failed. unsupported_execution";
  const observedHash = createHash("sha256").update(observedOutput).digest("hex");
  await writeFile(
    preflight,
    `#!/usr/bin/env bash
set -eu
: >"$PREFLIGHT_MARKER"
printf '%s\\n' '{"schema_version":1,"agent":"codex","status":"preflight_rejected","disposition":"not_sent","error_code":"unsupported_execution","exit_code":2,"observed_output":"${observedOutput}","observed_output_sha256":"sha256:${observedHash}","counters":{"provision_attempts":0,"network_requests":0,"state_entries_created":0}}'
`,
  );
  await chmod(preflight, 0o700);

  const reviewedManifest = JSON.parse(await readFile(fixture, "utf8"));
  const manifest = structuredClone(reviewedManifest);
  const manifestPath = join(temp, "manifest.json");
  await writeFile(manifestPath, JSON.stringify(manifest));
  const preflightPath = join(temp, "preflight.json");
  const preflightResult = await run(process.execPath, [script.pathname, "--preflight", "--manifest", manifestPath, "--managed-preflight", preflight, "--record", preflightPath], {
    ...process.env,
    PREFLIGHT_MARKER: marker,
  });
  assert.equal(preflightResult.code, 0, preflightResult.stderr);
  const attachment = JSON.parse(await readFile(preflightPath, "utf8"));
  assert.equal(attachment.capture.source, "pillbox-managed-preflight");
  assert.deepEqual(Object.keys(attachment.capture).sort(), ["preflight", "source"]);
  assert.equal(attachment.capture.preflight.error_code, "unsupported_execution");
  assert.deepEqual(attachment.capture.preflight.counters, {
    provision_attempts: 0,
    network_requests: 0,
    state_entries_created: 0,
  });
  assert.equal(await fileExists(marker), true);

  const report = structuredClone(reviewedManifest);
  report.capture.source = "huddles-managed-burnin";
  report.capture.preflight = attachment.capture.preflight;
  report.capture.cleanup = null;
  report.capture.run_cost_envelopes[0].observed = null;
  report.capture.read_only = null;
  report.capture.totals = null;
  const reportPath = join(temp, "huddles-report.json");
  await writeFile(reportPath, JSON.stringify(report));
  const partial = await run(process.execPath, [reconciler.pathname, reportPath, "--partial"]);
  assert.equal(partial.code, 0, partial.stderr);
  assert.match(partial.stdout, /authoritative Huddles burn-in report-v3 partial passed/);

  const finalizePath = join(temp, "finalize.json");
  const finalizeRequest = {
    sessionId: manifest.workload.steps.find((step) => step.id === "finalize").session_id,
    workspace: {
      repo: {
        endpoint: "https://example.invalid",
        bucket: "burnin",
        prefix: "snapshots/",
        accessKeyId: "access-key",
        secretAccessKey: "secret-key",
      },
      password: "workspace-password",
      snapshot: "a".repeat(64),
    },
  };
  await writeFile(finalizePath, JSON.stringify(finalizeRequest));
  const requestDigest = digestCanonical(finalizeRequest);
  const finalizeId = digestCanonical({
    session_id: finalizeRequest.sessionId,
    request_digest: requestDigest,
  });
  const resultSnapshot = "c".repeat(64);
  const snapshotInspector = join(temp, "pillbox");
  const verificationCapture = syntheticCapture();
  const verificationFixturePath = join(temp, "verification-fixture.json");
  await writeFile(verificationFixturePath, JSON.stringify(verificationCapture));
  await writeFile(snapshotInspector, `#!/usr/bin/env bash
set -eu
test "$1" = snapshot
test "$2" = show
test "$4" = --json
test "$5" = --pillbox
test "$6" = burnin-pillbox
if test "\${7:-}" = --capture-r2; then
  umask 077
  cp "$BURNIN_VERIFICATION_CAPTURE" "$8"
fi
printf '{"version":1,"snapshot":{"handle":"%s","parents":["%s"]}}\\n' "$3" "$BURNIN_SNAPSHOT_PARENT"
`);
  await chmod(snapshotInspector, 0o700);

  const requests = [];
  const requestBodies = [];
  let finalizeEffects = 0;
  const server = createServer(async (request, response) => {
    const requestBody = await requestText(request);
    const authorization = request.headers.authorization;
    if (authorization === "Bearer failed-transfer-token") {
      const capture = syntheticCapture("snapshot_finalize");
      capture.status = "incomplete";
      capture.reasons = ["operation_failed"];
      response.writeHead(502, { "content-type": "application/json" });
      response.end(JSON.stringify({
        finalizeId, requestDigest, r2Capture: capture,
        error: { code: "workspace_transfer_failed" },
      }));
      return;
    }
    if (authorization === "Bearer wrong-digest-token") {
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({
        status: "completed",
        disposition: "created",
        finalizeId,
        requestDigest: `sha256:${"0".repeat(64)}`,
        resultSnapshot,
      }));
      return;
    }
    if (authorization === "Bearer wrong-parent-token") {
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({
        status: "completed",
        disposition: "created",
        finalizeId,
        requestDigest,
        resultSnapshot,
      }));
      return;
    }
    assert.equal(authorization, "Bearer finalize-token");
    requests.push(request.url);
    requestBodies.push(requestBody);
    if (request.url === "/v2/workspaces/finalize") {
      finalizeEffects += 1;
      request.socket.destroy();
      return;
    }
    assert.equal(request.url, "/v2/workspaces/finalize/status");
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify({
      status: "completed",
      disposition: "reused",
      finalizeId,
      requestDigest,
      resultSnapshot,
    }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const { port } = server.address();

  const finalizeArgs = [script.pathname, "--finalize", "--manifest", manifestPath, "--report", reportPath, "--finalize-request", finalizePath, "--base-url", `http://127.0.0.1:${port}`, "--pillbox", "burnin-pillbox"];
  const finalizeEnv = {
    ...process.env,
    PATH: `${temp}:${process.env.PATH}`,
    BURNIN_CONFIRM_ISOLATED: "1",
    BURNIN_SNAPSHOT_PARENT: finalizeRequest.workspace.snapshot,
    BURNIN_VERIFICATION_CAPTURE: verificationFixturePath,
  };

  const mismatchedDigest = await run(process.execPath, finalizeArgs, {
    ...finalizeEnv,
    BURNIN_FINALIZE_TOKEN: "wrong-digest-token",
  });
  assert.equal(mismatchedDigest.code, 1);
  assert.match(mismatchedDigest.stderr, /canonical request digest, finalize ID/);
  assert.equal(JSON.parse(await readFile(reportPath, "utf8")).capture.cleanup, null);

  const mismatchedParent = await run(process.execPath, finalizeArgs, {
    ...finalizeEnv,
    BURNIN_FINALIZE_TOKEN: "wrong-parent-token",
    BURNIN_SNAPSHOT_PARENT: "b".repeat(64),
  });
  assert.equal(mismatchedParent.code, 1);
  assert.match(mismatchedParent.stderr, /not descended from the requested base snapshot/);
  assert.equal(JSON.parse(await readFile(reportPath, "utf8")).capture.cleanup, null);

  const occupiedCapture = join(temp, "occupied-capture.json");
  await writeFile(occupiedCapture, "existing evidence");
  const blockedCapture = await run(process.execPath, [...finalizeArgs, "--operation-capture-out", occupiedCapture], {
    ...finalizeEnv, BURNIN_FINALIZE_TOKEN: "finalize-token",
  });
  assert.equal(blockedCapture.code, 1);
  assert.match(blockedCapture.stderr, /no finalize request sent/);
  assert.equal(requests.length, 0);
  assert.equal(await readFile(occupiedCapture, "utf8"), "existing evidence");

  const operationCapture = join(temp, "operation-capture.json");
  const failedCapturePath = join(temp, "failed-operation-capture.json");
  const failedTransfer = await run(process.execPath, [...finalizeArgs, "--operation-capture-out", failedCapturePath], {
    ...finalizeEnv, BURNIN_FINALIZE_TOKEN: "failed-transfer-token",
  });
  assert.equal(failedTransfer.code, 1);
  assert.match(failedTransfer.stderr, /HTTP 502/);
  const failedCapture = JSON.parse(await readFile(failedCapturePath, "utf8"));
  assert.equal(failedCapture.captures.snapshot_finalize.status, "incomplete");
  assert.deepEqual(failedCapture.captures.snapshot_finalize.reasons, ["operation_failed"]);
  assert.deepEqual(failedCapture.captures.verification, { status: "unavailable", reason: "not_observed" });
  assert.equal(JSON.parse(await readFile(reportPath, "utf8")).capture.cleanup, null);

  const finalizeResult = await run(process.execPath, [...finalizeArgs, "--operation-capture-out", operationCapture], {
    ...finalizeEnv,
    BURNIN_FINALIZE_TOKEN: "finalize-token",
  });
  assert.equal(finalizeResult.code, 0, finalizeResult.stderr);
  const completed = JSON.parse(await readFile(reportPath, "utf8"));
  assert.equal(completed.capture.cleanup.step_id, "finalize");
  assert.equal(completed.capture.cleanup.session_id, completed.capture.execution_identity.session_id);
  assert.equal(completed.capture.cleanup.request_count, 2);
  assert.equal(completed.capture.cleanup.disposition, "reused");
  assert.equal(completed.capture.cleanup.request_digest, requestDigest);
  assert.equal(completed.capture.cleanup.finalize_id, finalizeId);
  assert.equal(completed.capture.cleanup.requested_base_snapshot, finalizeRequest.workspace.snapshot);
  assert.equal(completed.capture.cleanup.result_snapshot, resultSnapshot);
  assert.deepEqual(requests, ["/v2/workspaces/finalize", "/v2/workspaces/finalize/status"]);
  assert.equal(requestBodies[0], requestBodies[1]);
  assert.equal(finalizeEffects, 1);
  const sidecar = JSON.parse(await readFile(operationCapture, "utf8"));
  assert.deepEqual(sidecar.execution_identity, report.capture.execution_identity);
  assert.equal(sidecar.finalize_id, finalizeId);
  assert.equal(sidecar.request_digest, requestDigest);
  assert.deepEqual(sidecar.captures.snapshot_finalize, { status: "unavailable", reason: "replayed_without_capture" });
  assert.deepEqual(sidecar.captures.verification, verificationCapture);
  assert.equal((await stat(operationCapture)).mode & 0o777, 0o600);
  assert.doesNotMatch(await readFile(operationCapture, "utf8"), /workspace-password|secret-key|access-key|finalize-token/);
  assert.deepEqual(completed.capture.runtime_calls, report.capture.runtime_calls);
  assert.deepEqual(completed.capture.run_cost_envelopes[0].execution_identity, completed.capture.execution_identity);

  const duplicateFinalize = await run(process.execPath, finalizeArgs, {
    ...finalizeEnv,
    BURNIN_FINALIZE_TOKEN: "finalize-token",
  });
  assert.equal(duplicateFinalize.code, 1);
  assert.match(duplicateFinalize.stderr, /cleanup still pending/);
  assert.equal(requests.length, 2, "cleanup may attach only once and never predate execution");

  completed.capture.run_cost_envelopes[0].observed = reviewedManifest.capture.run_cost_envelopes[0].observed;
  completed.capture.read_only = reviewedManifest.capture.read_only;
  completed.capture.totals = reviewedManifest.capture.totals;
  completed.capture.read_only.worker.requests += 1;
  completed.capture.totals.worker.requests += 1;
  completed.capture.read_only.d1.rows_read += 1;
  completed.capture.totals.d1.rows_read += 1;
  const receipt = JSON.parse(await readFile(new URL("./testdata/cost-receipt.fixture.json", import.meta.url), "utf8"));
  // This fake finalizer adds one status poll beyond the fixed fixture. Attribute
  // it in both independent scope totals; it is not a new execution or artifact.
  receipt.accounting.d1.finalize.units.rows_read += 1;
  receipt.accounting.d1.finalize.source.record_ids.push("synthetic-finalize-status");
  receipt.accounting.d1.total.units.rows_read += 1;
  receipt.accounting.workers.runtime.units.requests += 1;
  receipt.accounting.workers.runtime.source.record_ids.push("synthetic-finalize-status");
  receipt.accounting.workers.total.units.requests += 1;
  const receiptPath = join(temp, "receipt.json");
  await writeFile(receiptPath, JSON.stringify(receipt));
  await writeFile(reportPath, JSON.stringify(completed));
  const reconciliation = await run(process.execPath, [reconciler.pathname, reportPath,
    "--receipt", receiptPath]);
  assert.equal(reconciliation.code, 0, reconciliation.stderr);
  assert.match(reconciliation.stdout, /managed preview burn-in reconciliation passed/);
});

test("operation sidecar preserves failed verification without accepting unsafe telemetry", async t => {
  const directory = await mkdtemp(join(tmpdir(), "pillbox-sidecar-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const output = join(directory, "operations.json");
  const sidecar = await createOperationSidecar(output, { finalize_id: "synthetic" }, { bucket: "burnin", prefix: "snapshots/" });
  try {
    const final = syntheticCapture("snapshot_finalize");
    await sidecar.recordFinalize(final, false);
    assert.deepEqual(JSON.parse(await readFile(output, "utf8")).captures.snapshot_finalize, final);
    await sidecar.recordFinalize({ ...final, authorization: "never-copy-this-token" }, false);
    assert.doesNotMatch(await readFile(output, "utf8"), /never-copy-this-token/);
    const incomplete = { ...syntheticCapture(), status: "incomplete", reasons: ["operation_failed"] };
    await assert.rejects(sidecar.verify(async path => {
      await writeFile(path, JSON.stringify(incomplete));
      throw new Error("verification failed");
    }), /verification failed/);
    assert.deepEqual(JSON.parse(await readFile(output, "utf8")).captures.verification, incomplete);
    await sidecar.verify(async path => { await writeFile(path, "x".repeat(512 * 1024 + 1)); });
    assert.equal(JSON.parse(await readFile(output, "utf8")).captures.verification.status, "unavailable");
  } finally { await sidecar.close(); }
});

async function requestText(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8");
}

function digestCanonical(value) {
  return `sha256:${createHash("sha256").update(canonicalJson(value)).digest("hex")}`;
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort((left, right) => left < right ? -1 : left > right ? 1 : 0)
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

async function fileExists(path) {
  try {
    await readFile(path);
    return true;
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}

function run(command, args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { env });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, stdout, stderr }));
  });
}
