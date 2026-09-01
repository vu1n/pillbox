import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const script = new URL("./scripts/burnin-fixed-workload.mjs", import.meta.url);
const fixture = new URL("./testdata/burnin-reconciliation.fixture.json", import.meta.url);

test("live burn-in records the executable managed preflight before network access", async (t) => {
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

  const manifest = JSON.parse(await readFile(fixture, "utf8"));
  const unsupported = manifest.workload.steps.find((step) => step.id === "unsupported-managed-codex");
  unsupported.expected = {
    error_code: "must-not-be-copied",
    provision_attempts: 41,
    network_requests: 42,
  };
  const manifestPath = join(temp, "manifest.json");
  await writeFile(manifestPath, JSON.stringify(manifest));
  const finalizePath = join(temp, "finalize.json");
  await writeFile(
    finalizePath,
    JSON.stringify({
      sessionId: manifest.workload.steps.find((step) => step.id === "finalize").session_id,
    }),
  );

  const requestHash = `sha256:${"b".repeat(64)}`;
  const artifactKey = "executions/burnin-invocation-1/evidence.json";
  const cost = manifest.capture.run_cost_envelopes[0].cost;
  let executeCalls = 0;
  const requests = [];
  const server = createServer(async (request, response) => {
    assert.equal(await fileExists(marker), true, "preflight must run before the first HTTP request");
    requests.push(request.url);
    for await (const _ of request) {
      // Drain the bounded request body before replying.
    }
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
      evidence: { artifact_ref: { key: artifactKey } },
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
  const observed = report.capture.responses[0];
  assert.equal(observed.step_id, "unsupported-managed-codex");
  assert.equal(observed.error_code, "unsupported_execution");
  assert.deepEqual(observed.counters, {
    provision_attempts: 0,
    network_requests: 0,
    state_entries_created: 0,
  });
  assert.match(observed.observed_output, /unsupported_execution/);
  assert.equal(requests.length, 5);
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
