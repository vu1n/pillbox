import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { validateCostReceipt } from "./scripts/cost-receipt.mjs";

const fixture = JSON.parse(await readFile(new URL("./testdata/cost-receipt.fixture.json", import.meta.url), "utf8"));
const report = JSON.parse(await readFile(new URL("./testdata/burnin-reconciliation.fixture.json", import.meta.url), "utf8"));
const run = report.capture.run_cost_envelopes[0];
const expected = { ...run, cost_ref: run.id, report };

test("receipt validates independent lifecycle usage without changing evidence", () => {
  const before = JSON.stringify({ fixture, expected });
  assert.deepEqual(validateCostReceipt(fixture, expected), []);
  assert.equal(JSON.stringify({ fixture, expected }), before);
});

test("real commit-order discrepancy reconciles measured reads, not a fixed correction", () => {
  const receipt = structuredClone(fixture);
  const context = structuredClone(expected);
  context.cost.infrastructure.d1_rows_read = 5;
  context.cost.infrastructure.d1_rows_written = 10;
  receipt.d1.pre_seal = { rows_read: 5, rows_written: 9 };
  for (const reads of [2, 3]) {
    receipt.d1.terminal_commit = { rows_read: reads, rows_written: 1 };
    receipt.d1.execution_total = { rows_read: 5 + reads, rows_written: 10 };
    context.observed.d1 = { ...receipt.d1.execution_total };
    receipt.accounting.d1.execution.units = { ...receipt.d1.execution_total };
    receipt.accounting.d1.total.units = {
      rows_read: receipt.d1.execution_total.rows_read + context.report.capture.read_only.d1.rows_read,
      rows_written: receipt.d1.execution_total.rows_written + context.report.capture.read_only.d1.rows_written,
    };
    assert.deepEqual(validateCostReceipt(receipt, context), []);
  }
});

test("malformed or unavailable receipt input fails without throwing", () => {
  for (const value of [undefined, null, [], 1, "receipt", {}, { d1: null, sources: [] }]) {
    assert.ok(validateCostReceipt(value, expected).length > 0);
  }
});

test("missing, foreign, unexplained, overlapping, and nonfinite evidence fails", () => {
  const mutations = [
    r => { delete r.sources; },
    r => { r.extra = 1; },
    r => { r.execution_identity.invocation_id = "other"; },
    r => { r.artifact_ref.sha256 = `sha256:${"e".repeat(64)}`; },
    r => { r.cost_ref = `sha256:${"f".repeat(64)}`; },
    r => { r.d1.pre_seal.rows_read = 1; },
    r => { r.d1.terminal_commit.rows_written = 2; },
    r => { r.d1.execution_total.rows_read = 3; },
    r => { r.d1.pre_seal.rows_written = Number.MAX_SAFE_INTEGER; },
    r => { r.container.cpu_seconds = null; },
    r => { r.container.memory_byte_seconds = Infinity; },
    r => { r.container.disk_byte_seconds = -1; },
    r => { r.container.egress_bytes = 0.5; },
    r => { r.container.profile = "other"; },
    r => { r.container.execution_duration_ms = 1; },
    r => { r.sources.container_lifecycle.resource_id = "other"; },
    r => { r.sources.d1_pre_seal.resource_id = "other"; },
    r => { r.sources.d1_pre_seal.capture_sha256 = "not-a-digest"; },
    r => { r.sources.d1_pre_seal.to = "2026-09-13T00:10:03.000Z"; },
    r => { r.sources.d1_terminal_commit.to = "2026-09-13T00:11:00.000Z"; },
    r => { r.sources.container_lifecycle.to = "2026-09-13T00:10:02.000Z"; },
    r => { r.sources.d1_pre_seal.from = "invalid"; },
    r => { r.sources.d1_pre_seal.to = r.sources.d1_pre_seal.from; },
  ];
  for (const mutate of mutations) {
    const receipt = structuredClone(fixture);
    mutate(receipt);
    assert.ok(validateCostReceipt(receipt, expected).length > 0, mutate.toString());
  }
});
