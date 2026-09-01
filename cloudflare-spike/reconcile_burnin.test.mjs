import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const reconciler = new URL("./scripts/reconcile-burnin.mjs", import.meta.url);
const fixture = new URL("./testdata/burnin-reconciliation.fixture.json", import.meta.url);

test("recorder-shaped fixture reconciles without translating response records", async () => {
  const result = await run(process.execPath, [reconciler.pathname, fixture.pathname]);
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /managed preview burn-in reconciliation passed/);
});

test("reconciliation fails when a runtime workload step is omitted", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.runtime_calls = report.capture.runtime_calls.filter(
    (call) => call.step_id !== "status-page-2",
  );
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /omitted workload step status-page-2/);
});

test("reconciliation fails when cleanup is attributed to the wrong step", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.cleanup.step_id = "unsupported-managed-codex";
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /capture\.cleanup does not match the workload cleanup step/);
});

async function reconcileTemporary(t, report) {
  const temp = await mkdtemp(join(tmpdir(), "pillbox-burnin-reconcile-"));
  t.after(() => rm(temp, { recursive: true, force: true }));
  const path = join(temp, "report.json");
  await writeFile(path, JSON.stringify(report));
  return run(process.execPath, [reconciler.pathname, path]);
}

function run(command, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args);
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, stdout, stderr }));
  });
}
