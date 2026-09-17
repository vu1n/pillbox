import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { validateCostReceipt } from "./scripts/cost-receipt.mjs";

const fixture = JSON.parse(await readFile(new URL("./testdata/cost-receipt.fixture.json", import.meta.url), "utf8"));
const report = JSON.parse(await readFile(new URL("./testdata/burnin-reconciliation.fixture.json", import.meta.url), "utf8"));
const validate = (receipt, context = report) => {
  const run = context.capture.run_cost_envelopes[0];
  return validateCostReceipt(receipt, { ...run, cost_ref: run.id, report: context });
};

test("overhead totals preserve the narrow artifact and runtime budgets", () => {
  const before = JSON.stringify({ fixture, report });
  assert.equal(fixture.accounting.r2.total.units.writes, 4);
  assert.equal(report.capture.totals.r2.writes, 1);
  assert.equal(fixture.accounting.workers.total.units.requests, 22);
  assert.equal(report.capture.totals.worker.requests, 7);
  assert.deepEqual(validate(fixture), []);
  assert.equal(JSON.stringify({ fixture, report }), before);
});

test("legacy receipts and missing zero-usage observations fail closed", () => {
  for (const mutate of [
    r => { r.schema_version = 1; },
    r => { delete r.accounting; },
    r => { r.accounting = null; },
    r => { r.accounting.d1.operator_inspection = null; },
    r => { r.accounting.d1.operator_inspection.source.record_ids = []; },
    r => { r.accounting.vendor_do.cleanup_idle.units.sql_rows_read = null; },
    r => { delete r.accounting.vendor_retention; },
  ]) {
    const receipt = structuredClone(fixture);
    mutate(receipt);
    assert.ok(validate(receipt).length > 0, mutate.toString());
  }
});

test("all scope units and independent totals are required", () => {
  for (const group of ["d1", "r2", "workers", "vendor_do"]) {
    for (const part of Object.keys(fixture.accounting[group])) {
      for (const key of Object.keys(fixture.accounting[group][part].units)) {
        const receipt = structuredClone(fixture);
        delete receipt.accounting[group][part].units[key];
        assert.ok(validate(receipt).length > 0, `${group}.${part}.${key}`);
      }
    }
    const extra = structuredClone(fixture);
    extra.accounting[group].unattributed = { units: {} };
    assert.ok(validate(extra).some(x => /not allowed/.test(x)));
    for (const key of Object.keys(fixture.accounting[group].total.units)) {
      const receipt = structuredClone(fixture);
      receipt.accounting[group].total.units[key] += 1;
      assert.ok(validate(receipt).some(x => /unexplained delta/.test(x)), `${group}.${key}`);
    }
  }
});

test("source records, selectors, resources, and windows cannot be substituted", () => {
  const mutations = [
    r => { r.accounting.r2.snapshot_finalize.source.record_ids = r.accounting.r2.artifact.source.record_ids; },
    r => { r.accounting.r2.total.source.record_ids = r.accounting.r2.artifact.source.record_ids; },
    r => { r.accounting.r2.snapshot_finalize.source.capture_sha256 = `sha256:${"d".repeat(64)}`; r.accounting.r2.snapshot_finalize.source.record_ids = r.accounting.r2.artifact.source.record_ids; },
    r => { r.accounting.d1.execution.source.dataset = "unrelated-dataset"; },
    r => { r.accounting.d1.execution.source.resource_id = "other-database"; },
    r => { r.accounting.vendor_do.total.source.resource_id = "other-object"; },
    r => { r.accounting.workers.issuer.source.resource_id = r.accounting.workers.runtime.source.resource_id; },
    r => { r.accounting.workers.runtime.source.resource_id = "other-worker"; },
    r => { r.accounting.r2.artifact.source.key_prefix = "executions/"; },
    r => { r.accounting.snapshot_prefix = "executions/other/"; },
    r => { r.accounting.snapshot_prefix = ""; },
    r => { r.accounting.r2.verification.source.key_prefix = "another-prefix/"; },
    r => { r.accounting.d1.finalize.source.to = "2026-09-13T01:00:00.000Z"; },
    r => { r.accounting.d1.execution.source.from = "invalid"; },
    r => { r.accounting.d1.execution.source.from = "2026-09-13T00:09:00.000Z"; },
    r => { r.accounting.d1.execution.source.capture_sha256 = "not-a-hash"; },
    r => { r.accounting.d1.execution.source.record_ids = ["same", "same"]; },
    r => { r.accounting.vendor_retention.after.source.resource_id = "another-namespace"; },
    r => { r.accounting.vendor_retention.after.source.from = "2026-09-13T00:08:00.000Z"; },
    r => { r.accounting.vendor_retention.after.source.record_ids = r.accounting.vendor_retention.before.source.record_ids; },
    r => { r.accounting.vendor_retention.before.source.from = "2026-09-13T00:09:00.000Z"; r.accounting.vendor_retention.before.source.to = "2026-09-13T00:09:30.000Z"; },
  ];
  for (const mutate of mutations) {
    const receipt = structuredClone(fixture);
    mutate(receipt);
    assert.ok(validate(receipt).length > 0, mutate.toString());
  }
});

test("malformed units and oversized record arrays return bounded validation failures", () => {
  for (const value of [{ toString: null }, [], null, "1", true]) {
    const receipt = structuredClone(fixture);
    receipt.accounting.workers.issuer.units.requests = value;
    assert.ok(validate(receipt).some(x => /safe integer/.test(x)));
  }
  const receipt = structuredClone(fixture);
  for (const part of ["before", "after"]) {
    receipt.accounting.vendor_retention[part].source.record_ids = Array.from({ length: 513 }, (_, i) => `${part}-${i}`);
  }
  assert.equal(validate(receipt).filter(x => /requires 1–512 records/.test(x)).length, 2);
});

test("snapshot selector must exclude the actual artifact even outside its normal namespace", () => {
  const receipt = structuredClone(fixture);
  const context = structuredClone(report);
  receipt.artifact_ref.key = "snapshots/result.json";
  receipt.accounting.r2.artifact.source.key_prefix = receipt.artifact_ref.key;
  context.capture.run_cost_envelopes[0].artifact_ref.key = receipt.artifact_ref.key;
  assert.ok(validate(receipt, context).some(x => /overlaps the recorded artifact/.test(x)));
});

test("balanced totals cannot hide writes in read-only scopes or extra artifacts", () => {
  for (const part of ["snapshot_restore", "verification", "bucket_probes", "artifact"]) {
    const receipt = structuredClone(fixture);
    receipt.accounting.r2[part].units.writes += 1;
    receipt.accounting.r2.total.units.writes += 1;
    assert.ok(validate(receipt).length > 0, part);
  }
  const receipt = structuredClone(fixture);
  receipt.accounting.d1.operator_inspection.units.rows_written = 1;
  receipt.accounting.d1.total.units.rows_written += 1;
  assert.ok(validate(receipt).some(x => /inspection cannot write/.test(x)));
});

test("fractional lifecycle units retain precision but row arithmetic stays integral", () => {
  const receipt = structuredClone(fixture);
  receipt.accounting.vendor_do.execute.units.duration_gb_seconds = 0.1;
  receipt.accounting.vendor_do.cleanup_idle.units.duration_gb_seconds = 0.2;
  receipt.accounting.vendor_do.total.units.duration_gb_seconds = 0.3;
  assert.deepEqual(validate(receipt), []);
  for (const value of [NaN, Infinity, -1, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
    receipt.accounting.d1.operator_inspection.units.rows_read = value;
    assert.ok(validate(receipt).length > 0);
  }
});
