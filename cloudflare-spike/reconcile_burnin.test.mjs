import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const reconciler = new URL("./scripts/reconcile-burnin.mjs", import.meta.url);
const fixture = new URL(
  "./testdata/burnin-reconciliation.fixture.json",
  import.meta.url,
);

test("the sole Huddles recorder fixture reconciles without translating response records", async () => {
  const result = await run(process.execPath, [
    reconciler.pathname,
    fixture.pathname,
  ]);
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /managed preview burn-in reconciliation passed/);
});

test("partial validation accepts terminal Huddles evidence only before cleanup", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.cleanup = null;
  report.capture.run_cost_envelopes[0].observed = null;
  report.capture.read_only = null;
  report.capture.totals = null;
  const result = await reconcileTemporary(t, report, ["--partial"]);
  assert.equal(result.code, 0, result.stderr);
  assert.match(
    result.stdout,
    /authoritative Huddles burn-in report-v3 partial passed/,
  );
});

test("reconciliation rejects a second runtime recorder", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.source = "burnin-fixed-workload";
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /Huddles as the sole runtime recorder/);
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
  assert.match(
    result.stderr,
    /capture\.cleanup does not match the workload cleanup step/,
  );
});

test("reconciliation recomputes the full request and execution identity", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.workload.steps[0].request.execution_policy_revision =
    "managed/tampered";
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(
    result.stderr,
    /execution_identity does not match the canonical first-execute request/,
  );
});

test("reconciliation requires live allowance snapshots before and after execution", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.pre_execution_allowance_snapshot.reserved_executions = 1;
  report.capture.allowance_snapshot.reserved_executions = 0;
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(
    result.stderr,
    /pre_execution_allowance_snapshot does not prove reserved_executions 0/,
  );
  assert.match(
    result.stderr,
    /allowance_snapshot does not prove reserved_executions 1/,
  );
});

test("reconciliation binds invocation and session identity to the reviewed epoch", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.deployment.allowance_epoch = "burnin-other-epoch";
  report.capture.allowance_snapshot.deployment_epoch = "burnin-other-epoch";
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(
    result.stderr,
    /does not derive from the reviewed allowance epoch/,
  );
});

test("reconciliation rejects positional evidence discontinuity", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.runtime_calls[2].evidence.from = 1;
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /evidence\.from breaks positional continuity/);
});

test("reconciliation seals artifact identity and digest across every observation", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.runtime_calls[1].evidence.artifact_ref.sha256 = `sha256:${"e".repeat(64)}`;
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /used a second artifact identity or digest/);
});

test("reconciliation accepts a best-effort Analytics miss with explicit negative variance", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.run_cost_envelopes[0].observed.analytics_engine = {
    points_written: 0,
    variance_from_planned: -1,
  };
  report.capture.totals.analytics_engine.points_written = 0;
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 0, result.stderr);
  assert.match(
    result.stdout,
    /Analytics planned\/observed\/variance: 1\/0\/-1/,
  );
});

test("fixed topology requires exactly one planned Analytics point", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.run_cost_envelopes[0].analytics_points_planned = 0;
  report.capture.run_cost_envelopes[0].cost.infrastructure.analytics_points_planned = 0;
  report.capture.run_cost_envelopes[0].observed.analytics_engine.points_written = 0;
  report.capture.run_cost_envelopes[0].observed.analytics_engine.variance_from_planned = 0;
  report.capture.totals.analytics_engine.points_written = 0;
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(result.stderr, /must plan exactly one Analytics Engine point/);
  assert.match(result.stderr, /must be exactly one point/);
});

test("reconciliation rejects Analytics observations without truthful plan variance", async (t) => {
  const report = JSON.parse(await readFile(fixture, "utf8"));
  report.capture.run_cost_envelopes[0].observed.analytics_engine.variance_from_planned =
    -1;
  const result = await reconcileTemporary(t, report);
  assert.equal(result.code, 1);
  assert.match(
    result.stderr,
    /variance does not reconcile observed writes against planned units/,
  );
});

async function reconcileTemporary(t, report, args = []) {
  const temp = await mkdtemp(join(tmpdir(), "pillbox-burnin-reconcile-"));
  t.after(() => rm(temp, { recursive: true, force: true }));
  const path = join(temp, "report.json");
  await writeFile(path, JSON.stringify(report));
  return run(process.execPath, [reconciler.pathname, path, ...args]);
}

function run(command, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args);
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
