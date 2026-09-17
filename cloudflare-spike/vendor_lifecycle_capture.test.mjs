import assert from "node:assert/strict";
import { mkdtemp, readFile, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  MAX_RESPONSE_BYTES,
  PlanError,
  captureVendorLifecycle,
  runCli,
} from "./scripts/capture-vendor-lifecycle.mjs";

const ACCOUNT_TAG = "a".repeat(32);
const NAMESPACE_ID = "b".repeat(32);
const OBJECT_ID = "c".repeat(64);
const APPLICATION_ID = "00000000-0000-4000-8000-000000000000";
const INSTANCE_ID = "d".repeat(64);
const plan = {
  schema_version: 1,
  account_tag: ACCOUNT_TAG,
  namespace_id: NAMESPACE_ID,
  object_id: OBJECT_ID,
  application_id: APPLICATION_ID,
  instance_id: INSTANCE_ID,
  windows: {
    before: { from: "2026-09-15T00:00:00.000Z", to: "2026-09-15T00:10:00.000Z" },
    lifecycle: { from: "2026-09-15T00:10:00.000Z", to: "2026-09-15T00:20:00.000Z" },
    after: { from: "2026-09-15T00:20:00.000Z", to: "2026-09-15T00:30:00.000Z" },
  },
};

const doRow = (values = {}) => ({
  dimensions: { namespaceId: plan.namespace_id, objectId: plan.object_id, datetimeHour: "2026-09-15T00:00:00Z" },
  sum: { duration: 2.5, rowsRead: 3, rowsWritten: 1, ...values },
});
const containerRow = (values = {}) => ({
  dimensions: { applicationId: plan.application_id, instanceId: plan.instance_id, datetimeHour: "2026-09-15T00:00:00Z", location: "MUM", region: "APAC" },
  sum: { cpuTimeSec: 1.5, allocatedMemory: 100, allocatedDisk: 200, txBytes: 300, ...values },
});
const storageRow = (storedBytes = 4096) => ({
  dimensions: { namespaceId: plan.namespace_id, datetimeHour: "2026-09-15T00:00:00Z" },
  max: { storedBytes },
});

function body(key, rows, extra = {}) {
  return { data: { viewer: { accounts: [{ accountTag: plan.account_tag, [key]: rows }] } }, errors: null, ...extra };
}

function responseFromText(text, status = 200) {
  const bytes = Buffer.from(text, "utf8");
  let offset = 0;
  let cancelled = false;
  return {
    status,
    headers: { get: () => null },
    body: {
      getReader: () => ({
        read: async () => {
          if (cancelled || offset >= bytes.length) return { done: true, value: undefined };
          const value = bytes.subarray(offset, Math.min(offset + 8192, bytes.length));
          offset += value.length;
          return { done: false, value };
        },
        cancel: async () => { cancelled = true; },
      }),
    },
  };
}

function responseFor(value, status = 200) {
  return responseFromText(JSON.stringify(value), status);
}

function responseBodyWithDeepValue(value) {
  return body("durableObjectsPeriodicGroups", [doRow()], { nested: value });
}

function fakeFetch(overrides = {}) {
  const calls = [];
  const fetchImpl = async (url, options) => {
    calls.push({ url, options });
    const query = JSON.parse(options.body).query;
    let value;
    if (query.includes("durableObjectsPeriodicGroups")) value = overrides.do ?? body("durableObjectsPeriodicGroups", [doRow()]);
    else if (query.includes("containersUsageAdaptiveGroups")) value = overrides.container ?? body("containersUsageAdaptiveGroups", [containerRow()]);
    else if (query.includes("datetime_geq:\"2026-09-15T00:00:00.000Z\"")) value = overrides.before ?? body("durableObjectsSqlStorageGroups", [storageRow()]);
    else value = overrides.after ?? body("durableObjectsSqlStorageGroups", [storageRow()]);
    return responseFor(value, overrides.status ?? 200);
  };
  return { calls, fetchImpl };
}

test("malformed plans fail before the first network request", async () => {
  let calls = 0;
  const invalid = structuredClone(plan);
  invalid.account_tag = "not-an-account";
  await assert.rejects(
    captureVendorLifecycle(invalid, { token: "secret-token", fetchImpl: async () => { calls += 1; } }),
    PlanError,
  );
  assert.equal(calls, 0);
});

test("queries are bounded, filtered to exact resources, and retain no token", async () => {
  const { calls, fetchImpl } = fakeFetch();
  const result = await captureVendorLifecycle(plan, { token: "super-secret-token", fetchImpl, now: "2026-09-15T00:31:00.000Z" });
  assert.equal(calls.length, 4);
  assert.ok(calls.every(call => call.url === "https://api.cloudflare.com/client/v4/graphql"));
  assert.ok(calls.every(call => call.options.method === "POST" && call.options.redirect === "error"));
  assert.ok(calls.every(call => call.options.headers.authorization === "Bearer super-secret-token"));
  const queries = calls.map(call => JSON.parse(call.options.body).query).join("\n");
  assert.match(queries, new RegExp(`accountTag:"${ACCOUNT_TAG}"`));
  assert.match(queries, new RegExp(`namespaceId:"${NAMESPACE_ID}"`));
  assert.match(queries, new RegExp(`objectId:"${OBJECT_ID}"`));
  assert.match(queries, new RegExp(`applicationId:"${APPLICATION_ID}"`));
  assert.match(queries, new RegExp(`instanceId:"${INSTANCE_ID}"`));
  assert.match(queries, /limit:100/);
  assert.doesNotMatch(JSON.stringify(result), /super-secret-token|authorization/i);
  assert.equal(result.status, "available");
  assert.deepEqual(result.observations.do_lifecycle.units, { sql_rows_read: 3, sql_rows_written: 1, duration_gb_seconds: 2.5 });
  assert.equal(result.observations.container_lifecycle.units.tx_bytes, 300);
  assert.equal(result.sources.do_lifecycle.capture_sha256.length, 71);
});

test("explicit provider zeros remain observed zeros", async () => {
  const { fetchImpl } = fakeFetch({
    do: body("durableObjectsPeriodicGroups", [doRow({ duration: 0, rowsRead: 0, rowsWritten: 0 })]),
    container: body("containersUsageAdaptiveGroups", [containerRow({ cpuTimeSec: 0, allocatedMemory: 0, allocatedDisk: 0, txBytes: 0 })]),
    before: body("durableObjectsSqlStorageGroups", [storageRow(0)]),
    after: body("durableObjectsSqlStorageGroups", [storageRow(0)]),
  });
  const result = await captureVendorLifecycle(plan, { token: "test-token", fetchImpl });
  assert.equal(result.status, "available");
  assert.deepEqual(result.observations.do_lifecycle.units, { sql_rows_read: 0, sql_rows_written: 0, duration_gb_seconds: 0 });
  assert.deepEqual(result.observations.container_lifecycle.units, { cpu_time_sec: 0, allocated_memory: 0, allocated_disk: 0, tx_bytes: 0 });
  assert.equal(result.observations.retention_before.units.stored_bytes, 0);
});

test("empty, missing, GraphQL-error, truncated, and wrong-identifier rows are unavailable", async t => {
  const cases = [
    ["empty", { do: body("durableObjectsPeriodicGroups", []) }, "do_lifecycle", "empty_rows"],
    ["missing", { container: body("otherRows", [containerRow()]) }, "container_lifecycle", "missing_rows"],
    ["graphql error", { do: body("durableObjectsPeriodicGroups", [doRow()], { errors: [{ message: "provider rejected query" }] }) }, "do_lifecycle", "graphql_error"],
    ["row cap", { before: body("durableObjectsSqlStorageGroups", Array.from({ length: 100 }, () => storageRow())) }, "retention_before", "row_limit"],
    ["wrong identifier", { container: body("containersUsageAdaptiveGroups", [{ ...containerRow(), dimensions: { ...containerRow().dimensions, instanceId: "f".repeat(64) } }]) }, "container_lifecycle", "wrong_identifier"],
    ["malformed numeric", { do: body("durableObjectsPeriodicGroups", [doRow({ rowsRead: 1.5 })]) }, "do_lifecycle", "missing_or_invalid_numeric"],
    ["wrong account", { do: { data: { viewer: { accounts: [{ accountTag: "f".repeat(32), durableObjectsPeriodicGroups: [doRow()] }] } }, errors: null } }, "do_lifecycle", "wrong_identifier"],
  ];
  for (const [name, overrides, observation, reason] of cases) {
    await t.test(name, async () => {
      const { fetchImpl } = fakeFetch(overrides);
      const result = await captureVendorLifecycle(plan, { token: "test-token", fetchImpl });
      assert.equal(result.status, "unavailable");
      assert.equal(result.observations[observation].status, "unavailable");
      assert.equal(result.observations[observation].reason, reason);
      assert.equal(result.sources[observation].status, "unavailable");
      assert.equal(result.sources[observation].reason, reason);
      assert.equal(Object.hasOwn(result.observations[observation], "units"), false);
    });
  }
});

test("deep JSON responses become bounded unavailable metadata", async () => {
  let value = { value: true };
  for (let index = 0; index < 40; index += 1) value = { nested: value };
  const { fetchImpl } = fakeFetch({ do: responseBodyWithDeepValue(value) });
  const result = await captureVendorLifecycle(plan, { token: "test-token", fetchImpl });
  assert.equal(result.observations.do_lifecycle.reason, "response_depth");
  assert.equal(result.sources.do_lifecycle.response.bounded, false);
});

test("oversized responses are bounded and do not preserve an unbounded body", async () => {
  const { fetchImpl } = fakeFetch();
  let calls = 0;
  const boundedFetch = async (url, options) => {
    calls += 1;
    const query = JSON.parse(options.body).query;
    if (query.includes("durableObjectsPeriodicGroups")) {
      return responseFromText("x".repeat(MAX_RESPONSE_BYTES + 1));
    }
    return fetchImpl(url, options);
  };
  const result = await captureVendorLifecycle(plan, { token: "test-token", fetchImpl: boundedFetch });
  assert.equal(calls, 4);
  assert.equal(result.observations.do_lifecycle.reason, "response_too_large");
  assert.equal(result.sources.do_lifecycle.response.bounded, false);
  assert.ok(JSON.stringify(result).length < MAX_RESPONSE_BYTES);
});

test("CLI creates an exclusive 0600 output before a tokenless capture", async () => {
  const directory = await mkdtemp(join(tmpdir(), "vendor-lifecycle-"));
  const planPath = join(directory, "plan.json");
  const outPath = join(directory, "capture.json");
  await writeFile(planPath, JSON.stringify(plan));
  assert.equal(await runCli(["--plan", planPath, "--out", outPath], {}), 1);
  const info = await stat(outPath);
  assert.equal(info.mode & 0o777, 0o600);
  const output = JSON.parse(await readFile(outPath, "utf8"));
  assert.equal(output.status, "unavailable");
  assert.equal(output.sources.do_lifecycle.reason, "missing_token");
  assert.doesNotMatch(await readFile(outPath, "utf8"), /CLOUDFLARE_API_TOKEN|authorization|Bearer|secret/i);
  assert.equal(await runCli(["--plan", planPath, "--out", outPath], {}), 2);
});

test("aggregate overflow remains unavailable even when individual rows are safe", async () => {
  const { fetchImpl } = fakeFetch({
    do: body("durableObjectsPeriodicGroups", [doRow({ rowsRead: Number.MAX_SAFE_INTEGER }), doRow({ rowsRead: 1 })]),
    container: body("containersUsageAdaptiveGroups", [containerRow({ txBytes: Number.MAX_SAFE_INTEGER }), containerRow({ txBytes: 1 })]),
  });
  const result = await captureVendorLifecycle(plan, { token: "test-token", fetchImpl });
  assert.equal(result.observations.do_lifecycle.reason, "numeric_overflow");
  assert.equal(result.observations.container_lifecycle.reason, "numeric_overflow");
});

test("hung fetches time out and abort without retries", async () => {
  const signals = [];
  const result = await captureVendorLifecycle(plan, {
    token: "test-token", timeoutMs: 5,
    fetchImpl: (_url, options) => { signals.push(options.signal); return new Promise(() => {}); },
  });
  assert.equal(signals.length, 4);
  assert.ok(signals.every(signal => signal.aborted));
  assert.ok(Object.values(result.observations).every(value => value.reason === "timeout"));
});

test("a stalled response body is bounded and an oversized header cancels its request", async () => {
  for (const oversized of [false, true]) {
    const signals = [];
    const result = await captureVendorLifecycle(plan, {
      token: "test-token", timeoutMs: 5,
      fetchImpl: async (_url, options) => {
        signals.push(options.signal);
        return {
          status: 200,
          headers: { get: () => oversized ? String(MAX_RESPONSE_BYTES + 1) : null },
          body: { getReader: () => ({ read: () => new Promise(() => {}), cancel: async () => {} }) },
        };
      },
    });
    assert.ok(signals.every(signal => signal.aborted));
    assert.ok(Object.values(result.observations).every(value => value.reason === (oversized ? "response_too_large" : "timeout")));
  }
});
