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
  await writeFile(snapshotInspector, `#!/usr/bin/env bash
set -eu
test "$1" = snapshot
test "$2" = show
test "$4" = --json
test "$5" = --pillbox
test "$6" = burnin-pillbox
printf '{"version":1,"snapshot":{"handle":"%s","parents":["%s"]}}\\n' "$3" "$BURNIN_SNAPSHOT_PARENT"
`);
  await chmod(snapshotInspector, 0o700);

  const requests = [];
  const requestBodies = [];
  let finalizeEffects = 0;
  const server = createServer(async (request, response) => {
    const requestBody = await requestText(request);
    const authorization = request.headers.authorization;
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

  const finalizeResult = await run(process.execPath, finalizeArgs, {
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
  await writeFile(reportPath, JSON.stringify(completed));
  const reconciliation = await run(process.execPath, [reconciler.pathname, reportPath,
    "--receipt", new URL("./testdata/cost-receipt.fixture.json", import.meta.url).pathname]);
  assert.equal(reconciliation.code, 0, reconciliation.stderr);
  assert.match(reconciliation.stdout, /managed preview burn-in reconciliation passed/);
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
