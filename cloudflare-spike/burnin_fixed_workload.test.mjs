import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const script = new URL("./scripts/burnin-fixed-workload.mjs", import.meta.url);
const reconciler = new URL("./scripts/reconcile-burnin.mjs", import.meta.url);
const fixture = new URL("./testdata/burnin-reconciliation.fixture.json", import.meta.url);

test("live recorder output becomes a reconciled report after operator counters are attached", async (t) => {
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
  const finalizePath = join(temp, "finalize.json");
  await writeFile(
    finalizePath,
    JSON.stringify({
      sessionId: manifest.workload.steps.find((step) => step.id === "finalize").session_id,
    }),
  );

  const executeRequest = manifest.workload.steps.find((step) => step.id === "first-execute").request;
  const requestHash = digestOf(executeRequest);
  const executionDigest = digestOf({
    execution: executeRequest.execution,
    execution_policy_revision: executeRequest.execution_policy_revision,
  });
  const artifactKey = `executions/${createHash("sha256").update(executeRequest.invocation_id).digest("hex")}/${requestHash.slice("sha256:".length)}.json`;
  const artifactRef = {
    key: artifactKey,
    media_type: "application/json",
    bytes: 672,
    sha256: `sha256:${"d".repeat(64)}`,
  };
  const cost = manifest.capture.run_cost_envelopes[0].cost;
  let executeCalls = 0;
  const requests = [];
  const server = createServer(async (request, response) => {
    assert.equal(await fileExists(marker), true, "preflight must run before the first HTTP request");
    requests.push(request.url);
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString());
    response.setHeader("content-type", "application/json");
    if (request.url === "/v2/workspaces/finalize") {
      response.end(JSON.stringify({ resultSnapshot: "c".repeat(64) }));
      return;
    }
    if (request.url === "/v2/executions") executeCalls += 1;
    response.end(JSON.stringify({
      invocation_id: "burnin-invocation-1",
      status: "completed",
      disposition: executeCalls === 1 ? "created" : "reused",
      request_hash: requestHash,
      execution_digest: executionDigest,
      execution_policy_revision: executeRequest.execution_policy_revision,
      session_ref: { session_id: executeRequest.session_ref.session_id, seq_range: [0, 0] },
      evidence: {
        from: body.evidence_after ?? 0,
        next: null,
        truncated: false,
        events: (body.evidence_after ?? 0) === 0 ? [{ type: "message_delta" }] : [],
        artifact_ref: artifactRef,
      },
      cost,
    }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const { port } = server.address();

  const result = await run(process.execPath, [
    script.pathname,
    "--execute",
    "--manifest",
    manifestPath,
    "--finalize-request",
    finalizePath,
    "--managed-preflight",
    preflight,
    "--base-url",
    `http://127.0.0.1:${port}`,
  ], {
    ...process.env,
    BURNIN_CONFIRM_ISOLATED: "1",
    PREFLIGHT_MARKER: marker,
    BURNIN_EXECUTE_TOKEN: "execute-token",
    BURNIN_STATUS_1_TOKEN: "status-1-token",
    BURNIN_STATUS_2_TOKEN: "status-2-token",
    BURNIN_FINALIZE_TOKEN: "finalize-token",
  });
  assert.equal(result.code, 0, result.stderr);
  const report = JSON.parse(result.stdout);
  const observed = report.capture.preflight;
  assert.equal(observed.step_id, "unsupported-managed-codex");
  assert.equal(observed.error_code, "unsupported_execution");
  assert.deepEqual(observed.counters, {
    provision_attempts: 0,
    network_requests: 0,
    state_entries_created: 0,
  });
  assert.match(observed.observed_output, /unsupported_execution/);
  assert.equal(Object.hasOwn(observed, "artifact_key"), false);
  assert.equal(Object.hasOwn(observed, "cost_ref"), false);
  assert.equal(report.capture.cleanup.step_id, "finalize");
  assert.equal(Object.hasOwn(report.capture.cleanup, "artifact_key"), false);
  assert.equal(Object.hasOwn(report.capture.cleanup, "cost_ref"), false);
  assert.equal(
    report.capture.runtime_calls.length,
    manifest.workload.steps.filter((step) => step.operation === "execute" || step.operation === "status").length,
  );
  assert.equal(requests.length, manifest.workload.expected_network_requests);
  assert.equal(report.schema_version, 3);
  assert.equal(report.capture.execution_identity.request_hash, requestHash);
  assert.equal(report.capture.execution_identity.execution_digest, executionDigest);
  assert.deepEqual(report.capture.runtime_calls[0].session_ref.seq_range, [0, 0]);
  assert.equal(report.capture.runtime_calls[0].evidence.artifact_ref.sha256, artifactRef.sha256);
  assert.deepEqual(
    report.capture.run_cost_envelopes[0].execution_identity,
    report.capture.execution_identity,
  );

  report.capture.run_cost_envelopes[0].observed = reviewedManifest.capture.run_cost_envelopes[0].observed;
  report.capture.read_only = reviewedManifest.capture.read_only;
  report.capture.totals = reviewedManifest.capture.totals;
  const completedReport = join(temp, "completed-report.json");
  await writeFile(completedReport, JSON.stringify(report));
  const reconciliation = await run(process.execPath, [reconciler.pathname, completedReport], process.env);
  assert.equal(reconciliation.code, 0, reconciliation.stderr);
  assert.match(reconciliation.stdout, /managed preview burn-in reconciliation passed/);
});

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
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, stdout, stderr }));
  });
}

function digestOf(value) {
  return `sha256:${createHash("sha256").update(canonicalJson(value)).digest("hex")}`;
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value).sort((left, right) => left < right ? -1 : left > right ? 1 : 0)
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(",")}}`;
}
