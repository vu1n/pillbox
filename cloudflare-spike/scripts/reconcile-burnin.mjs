#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

const DEFAULT_FIXTURE = new URL(
  "../testdata/burnin-reconciliation.fixture.json",
  import.meta.url,
);
const MAX_STATUS_PAGE_SIZE = 100;
const TERMINAL_STATUSES = new Set([
  "completed",
  "failed",
  "cancelled",
  "interrupted",
]);

const fixturePath = process.argv[2] === undefined
  ? DEFAULT_FIXTURE
  : resolve(process.cwd(), process.argv[2]);

let fixture;
try {
  fixture = JSON.parse(await readFile(fixturePath, "utf8"));
} catch (error) {
  fail(`cannot read reconciliation fixture ${String(fixturePath)}: ${error}`);
}

const failures = [];
const check = (condition, message) => {
  if (!condition) failures.push(message);
};
const record = (value, path) => {
  check(isRecord(value), `${path} must be an object`);
  return isRecord(value) ? value : {};
};
const integer = (value, path) => {
  const valid = Number.isSafeInteger(value) && value >= 0;
  check(valid, `${path} must be a non-negative safe integer`);
  return valid ? value : 0;
};
const string = (value, path) => {
  check(typeof value === "string" && value.length > 0, `${path} must be non-empty`);
  return typeof value === "string" ? value : "";
};
const digest = (value, path) => {
  check(typeof value === "string" && /^sha256:[0-9a-f]{64}$/.test(value), `${path} must be sha256:<64 lowercase hex>`);
  return typeof value === "string" ? value : "";
};

const root = record(fixture, "fixture");
check(root.schema_version === 1, "fixture.schema_version must be 1");
const deployment = record(root.deployment, "fixture.deployment");
const workload = record(root.workload, "fixture.workload");
const capture = record(root.capture, "fixture.capture");

const workerName = string(deployment.worker_name, "deployment.worker_name");
check(workerName === "pillbox-managed-burnin", "deployment.worker_name must identify the isolated burn-in Worker");
check(deployment.container_class === "Sandbox", "deployment.container_class must be the vendor Sandbox class");
check(deployment.managed_execution_enabled === "1", "captured burn-in must have explicitly enabled managed execution");
const allowanceLimit = integer(deployment.managed_execution_limit, "deployment.managed_execution_limit");
const reviewedCount = integer(deployment.reviewed_new_execution_count, "deployment.reviewed_new_execution_count");
check(allowanceLimit === reviewedCount, "managed execution limit must equal the reviewed new-execution count");
check(allowanceLimit === 1, "this fixed burn-in is intentionally limited to one new managed execution");
check(integer(deployment.reserved_executions, "deployment.reserved_executions") === reviewedCount, "allowance reservation must equal the reviewed execution count");
const deploymentClasses = array(deployment.custom_durable_object_classes, "deployment.custom_durable_object_classes");
check(deploymentClasses.length === 0, "custom Durable Object classes are forbidden");

const steps = array(workload.steps, "workload.steps");
const stepById = new Map();
for (const [index, rawStep] of steps.entries()) {
  const step = record(rawStep, `workload.steps[${index}]`);
  const id = string(step.id, `workload.steps[${index}].id`);
  check(!stepById.has(id), `workload.steps contains duplicate id ${id}`);
  stepById.set(id, step);
}

const first = stepById.get("first-execute");
const retry = stepById.get("exact-retry");
const statusSteps = [stepById.get("status-page-1"), stepById.get("status-page-2")];
const codex = stepById.get("unsupported-managed-codex");
const finalize = stepById.get("finalize");
check(first?.operation === "execute", "workload must start with first-execute");
check(retry?.operation === "execute" && retry?.request_ref === "first-execute", "exact-retry must reuse the first execute request");
for (const [index, step] of statusSteps.entries()) {
  const path = `status-page-${index + 1}`;
  check(step?.operation === "status", `${path} must be a status read`);
  const limit = integer(step?.evidence_limit, `${path}.evidence_limit`);
  check(limit > 0 && limit <= MAX_STATUS_PAGE_SIZE, `${path}.evidence_limit must be between 1 and ${MAX_STATUS_PAGE_SIZE}`);
  integer(step?.evidence_after, `${path}.evidence_after`);
}
check(codex?.operation === "managed_codex_preflight", "unsupported-managed-codex must be an explicit preflight step");
check(record(codex?.expected, "unsupported-managed-codex.expected").error_code === "unsupported_execution", "managed Codex preflight must fail with unsupported_execution");
check(integer(codex?.expected?.provision_attempts, "unsupported-managed-codex.expected.provision_attempts") === 0, "unsupported managed Codex must not provision a container");
check(integer(codex?.expected?.network_requests, "unsupported-managed-codex.expected.network_requests") === 0, "unsupported managed Codex preflight must make no network request");
check(finalize?.operation === "workspace_finalize", "workload must include workspace finalize");
check(finalize?.requires_operator_request === true, "finalize must require an operator-supplied scoped request");
check(record(finalize?.expected, "finalize.expected").kill_before_transfer === true, "finalize must quiesce the container before transfer credentials enter");

const expectedNetworkRequests = integer(workload.expected_network_requests, "workload.expected_network_requests");
check(expectedNetworkRequests === 5, "fixed workload network request count must be five (execute, retry, two status pages, finalize)");
check(expectedNetworkRequests === 1 + 1 + statusSteps.filter(Boolean).length + 1, "workload network request count has an unexplained operation");

if (isRecord(first?.request)) {
  const request = first.request;
  const input = string(request.rendered_input, "first-execute.request.rendered_input");
  const expectedInputHash = `sha256:${createHash("sha256").update(input).digest("hex")}`;
  check(request.rendered_input_hash === expectedInputHash, "first-execute rendered_input_hash does not match the sealed input");
  check(request.tool_policy === "deny_all", "fixed burn-in must deny managed runtime tools");
  check(request.execution?.placement === "managed_container", "first-execute must target managed_container");
  check(request.execution?.transport?.harness === "opencode", "first-execute must use the supported managed OpenCode harness");
}

const responses = array(capture.responses, "capture.responses");
const responseByStep = new Map();
for (const [index, rawResponse] of responses.entries()) {
  const response = record(rawResponse, `capture.responses[${index}]`);
  const stepId = string(response.step_id, `capture.responses[${index}].step_id`);
  check(!responseByStep.has(stepId), `capture.responses contains duplicate step ${stepId}`);
  responseByStep.set(stepId, response);
  digest(response.request_hash, `capture.responses[${index}].request_hash`);
  string(response.artifact_key, `capture.responses[${index}].artifact_key`);
  string(response.cost_ref, `capture.responses[${index}].cost_ref`);
}
const firstResponse = responseByStep.get("first-execute");
check(firstResponse?.disposition === "created", "first execute must create the only execution claim");
check(firstResponse?.status === "completed", "first execute fixture must complete");
for (const stepId of ["exact-retry", "status-page-1", "status-page-2"]) {
  const response = responseByStep.get(stepId);
  check(response?.disposition === "reused", `${stepId} must reuse the first terminal result`);
  check(response?.status === "completed", `${stepId} must observe the completed terminal result`);
  check(response?.request_hash === firstResponse?.request_hash, `${stepId} changed the request hash during replay`);
  check(response?.artifact_key === firstResponse?.artifact_key, `${stepId} used a second artifact`);
  check(response?.cost_ref === firstResponse?.cost_ref, `${stepId} used a second cost envelope`);
}
check(responses.length === 4, "the fixture must capture exactly four execution/status responses");

const runs = array(capture.run_cost_envelopes, "capture.run_cost_envelopes");
check(runs.length === reviewedCount, "there must be exactly one captured RunCostEnvelope per genuinely new execution");
const runIds = new Set();
const observedRuns = [];
for (const [index, rawRun] of runs.entries()) {
  const run = record(rawRun, `capture.run_cost_envelopes[${index}]`);
  const runId = string(run.id, `capture.run_cost_envelopes[${index}].id`);
  const invocationId = string(run.invocation_id, `capture.run_cost_envelopes[${index}].invocation_id`);
  check(!runIds.has(runId), `duplicate RunCostEnvelope id ${runId}`);
  runIds.add(runId);
  check(invocationId === first?.invocation_id, `${runId} is not attributed to first-execute`);
  const cost = validateCost(run.cost, `${runId}.cost`);
  const observed = record(run.observed, `${runId}.observed`);
  const d1 = record(observed.d1, `${runId}.observed.d1`);
  const r2 = record(observed.r2, `${runId}.observed.r2`);
  const container = record(observed.container, `${runId}.observed.container`);
  const worker = record(observed.worker, `${runId}.observed.worker`);
  const analytics = record(observed.analytics_engine, `${runId}.observed.analytics_engine`);
  const vendor = validateVendorCounters(observed.vendor_sandbox_do, `${runId}.observed.vendor_sandbox_do`);
  const infra = record(cost.infrastructure, `${runId}.cost.infrastructure`);
  check(integer(run.artifact_count, `${runId}.artifact_count`) === 1, `${runId} must have exactly one immutable R2 artifact`);
  check(integer(run.analytics_point_count, `${runId}.analytics_point_count`) <= 1, `${runId} emitted more than one Analytics Engine point`);
  check(integer(run.analytics_point_count, `${runId}.analytics_point_count`) === infra.analytics_points_written, `${runId} Analytics capture disagrees with its RunCostEnvelope`);
  check(integer(run.artifact_count, `${runId}.artifact_count`) === integer(infra.r2_writes, `${runId}.cost.infrastructure.r2_writes`), `${runId} R2 artifact count disagrees with its RunCostEnvelope`);
  check(integer(infra.d1_rows_read, `${runId}.cost.infrastructure.d1_rows_read`) === integer(d1.rows_read, `${runId}.observed.d1.rows_read`), `${runId} D1 read delta is unexplained`);
  check(integer(infra.d1_rows_written, `${runId}.cost.infrastructure.d1_rows_written`) === integer(d1.rows_written, `${runId}.observed.d1.rows_written`), `${runId} D1 write delta is unexplained`);
  check(integer(infra.r2_reads, `${runId}.cost.infrastructure.r2_reads`) === integer(r2.reads, `${runId}.observed.r2.reads`), `${runId} R2 read delta is unexplained`);
  check(integer(infra.r2_writes, `${runId}.cost.infrastructure.r2_writes`) === integer(r2.writes, `${runId}.observed.r2.writes`), `${runId} R2 write delta is unexplained`);
  check(integer(infra.r2_bytes_read, `${runId}.cost.infrastructure.r2_bytes_read`) === integer(r2.bytes_read, `${runId}.observed.r2.bytes_read`), `${runId} R2 read-byte delta is unexplained`);
  check(integer(infra.r2_bytes_written, `${runId}.cost.infrastructure.r2_bytes_written`) === integer(r2.bytes_written, `${runId}.observed.r2.bytes_written`), `${runId} R2 write-byte delta is unexplained`);
  check(integer(infra.analytics_points_written, `${runId}.cost.infrastructure.analytics_points_written`) === integer(analytics.points_written, `${runId}.observed.analytics_engine.points_written`), `${runId} Analytics Engine delta is unexplained`);
  check(integer(infra.sandbox_duration_ms, `${runId}.cost.infrastructure.sandbox_duration_ms`) === integer(container.duration_ms, `${runId}.observed.container.duration_ms`), `${runId} container duration disagrees with its RunCostEnvelope`);
  check(infra.sandbox_profile === container.profile, `${runId} container profile disagrees with its RunCostEnvelope`);
  check(integer(worker.requests, `${runId}.observed.worker.requests`) === 1, `${runId} must account for one Worker execute request`);
  check(vendor.class_name === "Sandbox", `${runId} must be attributed to the vendor Sandbox DO`);
  check(vendor.custom_classes.length === 0, `${runId} observed a custom Durable Object class`);
  check(vendor.custom_storage_bytes_delta === 0, `${runId} observed custom Durable Object storage growth`);
  observedRuns.push({ d1, r2, container, worker, analytics_engine: analytics, vendor });
}

const readOnly = counters(record(capture.read_only, "capture.read_only"), "capture.read_only");
const totals = counters(record(capture.totals, "capture.totals"), "capture.totals");
const expectedTotals = sumCounters(observedRuns, readOnly);
compareCounters(expectedTotals, totals);
check(totals.worker.requests === expectedNetworkRequests, "captured Worker requests do not match the fixed workload");
check(totals.analytics_engine.points_written === runs.length, "captured Analytics Engine points exceed one per new terminal run");
check(totals.r2.writes === runs.length, "captured R2 writes exceed one immutable artifact per new terminal run");
check(totals.vendor_sandbox_do.custom_classes.length === 0, "captured topology contains a custom Durable Object class");
check(totals.vendor_sandbox_do.custom_storage_bytes_delta === 0, "captured custom Durable Object storage grew");
check(
  totals.r2.bytes_read === totals.r2.reads * (runs[0]?.observed?.r2?.bytes_written ?? 0),
  "captured R2 read bytes do not equal bounded artifact re-reads",
);

if (failures.length > 0) {
  console.error("✗ managed preview burn-in reconciliation failed");
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exitCode = 1;
} else {
  console.log("✓ managed preview burn-in reconciliation passed");
  console.log(`  isolated Worker: ${workerName}`);
  console.log(`  new executions: ${runs.length}/${allowanceLimit}`);
  console.log(`  artifacts: ${totals.r2.writes}; Analytics points: ${totals.analytics_engine.points_written}`);
  console.log(`  custom Durable Object classes: ${totals.vendor_sandbox_do.custom_classes.length}; custom storage delta: ${totals.vendor_sandbox_do.custom_storage_bytes_delta} bytes`);
}

function validateCost(value, path) {
  const cost = record(value, path);
  check(cost.version === 1, `${path}.version must be 1`);
  check(TERMINAL_STATUSES.has(cost.status), `${path}.status must be terminal`);
  const model = record(cost.model, `${path}.model`);
  for (const key of ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]) integer(model[key], `${path}.model.${key}`);
  if (model.provider_reported_cost_usd !== null) check(Number.isFinite(model.provider_reported_cost_usd) && model.provider_reported_cost_usd >= 0, `${path}.model.provider_reported_cost_usd must be null or non-negative`);
  const infra = record(cost.infrastructure, `${path}.infrastructure`);
  for (const key of ["d1_rows_read", "d1_rows_written", "r2_reads", "r2_writes", "r2_bytes_read", "r2_bytes_written", "analytics_points_written", "sandbox_duration_ms"]) integer(infra[key], `${path}.infrastructure.${key}`);
  check(infra.r2_writes <= 1, `${path}.infrastructure.r2_writes exceeds one artifact`);
  check(infra.analytics_points_written <= 1, `${path}.infrastructure.analytics_points_written exceeds one point`);
  check(typeof infra.sandbox_profile === "string" && infra.sandbox_profile.length > 0, `${path}.infrastructure.sandbox_profile must identify the captured profile`);
  check(cost.known_cost_usd === model.provider_reported_cost_usd, `${path}.known_cost_usd must equal provider-reported cost or both be null`);
  check(cost.estimated_total_cost_usd === null, `${path}.estimated_total_cost_usd requires a versioned rate card`);
  check(cost.rate_card_version === null, `${path}.rate_card_version must remain null without a rate card`);
  return cost;
}

function validateVendorCounters(value, path) {
  const vendor = record(value, path);
  check(vendor.class_name === "Sandbox", `${path}.class_name must be Sandbox`);
  for (const key of ["instances_started", "storage_reads", "storage_writes", "stored_bytes_before", "stored_bytes_after", "custom_storage_bytes_delta"]) integer(vendor[key], `${path}.${key}`);
  const customClasses = array(vendor.custom_classes, `${path}.custom_classes`);
  check(customClasses.length === 0, `${path}.custom_classes must be empty`);
  return { ...vendor, custom_classes: customClasses };
}

function counters(value, path) {
  const counters = record(value, path);
  const d1 = numericPair(counters.d1, `${path}.d1`, ["rows_read", "rows_written"]);
  const r2 = numericPair(counters.r2, `${path}.r2`, ["reads", "writes", "bytes_read", "bytes_written"]);
  const containerValue = record(counters.container, `${path}.container`);
  for (const key of ["instances_started", "duration_ms", "cleanup_calls"]) integer(containerValue[key], `${path}.container.${key}`);
  check(typeof containerValue.profile === "string" && containerValue.profile.length > 0, `${path}.container.profile must identify the captured profile`);
  const worker = numericPair(counters.worker, `${path}.worker`, ["requests"]);
  const analytics = numericPair(counters.analytics_engine, `${path}.analytics_engine`, ["points_written"]);
  const vendor = validateVendorCounters(counters.vendor_sandbox_do, `${path}.vendor_sandbox_do`);
  return { d1, r2, container: containerValue, worker, analytics_engine: analytics, vendor_sandbox_do: vendor };
}

function sumCounters(runs, readOnly) {
  const runVendor = runs.map((run) => run.vendor);
  return {
    d1: {
      rows_read: sumNested(runs, "d1", "rows_read") + readOnly.d1.rows_read,
      rows_written: sumNested(runs, "d1", "rows_written") + readOnly.d1.rows_written,
    },
    r2: {
      reads: sumNested(runs, "r2", "reads") + readOnly.r2.reads,
      writes: sumNested(runs, "r2", "writes") + readOnly.r2.writes,
      bytes_read: sumNested(runs, "r2", "bytes_read") + readOnly.r2.bytes_read,
      bytes_written: sumNested(runs, "r2", "bytes_written") + readOnly.r2.bytes_written,
    },
    container: {
      instances_started: sumNested(runs, "container", "instances_started") + readOnly.container.instances_started,
      duration_ms: sumNested(runs, "container", "duration_ms") + readOnly.container.duration_ms,
      profile: readOnly.container.profile,
      cleanup_calls: sumNested(runs, "container", "cleanup_calls") + readOnly.container.cleanup_calls,
    },
    worker: { requests: sumNested(runs, "worker", "requests") + readOnly.worker.requests },
    analytics_engine: { points_written: sumNested(runs, "analytics_engine", "points_written") + readOnly.analytics_engine.points_written },
    vendor_sandbox_do: {
      class_name: readOnly.vendor_sandbox_do.class_name,
      instances_started: sumNested(runs, "vendor", "instances_started") + readOnly.vendor_sandbox_do.instances_started,
      storage_reads: sumNested(runs, "vendor", "storage_reads") + readOnly.vendor_sandbox_do.storage_reads,
      storage_writes: sumNested(runs, "vendor", "storage_writes") + readOnly.vendor_sandbox_do.storage_writes,
      stored_bytes_before: readOnly.vendor_sandbox_do.stored_bytes_before,
      stored_bytes_after: readOnly.vendor_sandbox_do.stored_bytes_after,
      custom_classes: [...new Set([...runVendor.flatMap((vendor) => vendor.custom_classes), ...readOnly.vendor_sandbox_do.custom_classes])],
      custom_storage_bytes_delta: sumNested(runs, "vendor", "custom_storage_bytes_delta") + readOnly.vendor_sandbox_do.custom_storage_bytes_delta,
    },
  };
}

function compareCounters(expected, actual) {
  for (const key of ["rows_read", "rows_written"]) check(expected.d1[key] === actual.d1[key], `captured D1 ${key} has an unexplained delta`);
  for (const key of ["reads", "writes", "bytes_read", "bytes_written"]) check(expected.r2[key] === actual.r2[key], `captured R2 ${key} has an unexplained delta`);
  for (const key of ["instances_started", "duration_ms", "cleanup_calls"]) check(expected.container[key] === actual.container[key], `captured Container ${key} has an unexplained delta`);
  check(expected.container.profile === actual.container.profile, "captured Container profile has an unexplained delta");
  check(expected.worker.requests === actual.worker.requests, "captured Worker requests have an unexplained delta");
  check(expected.analytics_engine.points_written === actual.analytics_engine.points_written, "captured Analytics Engine points have an unexplained delta");
  for (const key of ["instances_started", "storage_reads", "storage_writes", "custom_storage_bytes_delta"]) check(expected.vendor_sandbox_do[key] === actual.vendor_sandbox_do[key], `captured vendor Sandbox/DO ${key} has an unexplained delta`);
  check(expected.vendor_sandbox_do.stored_bytes_before === actual.vendor_sandbox_do.stored_bytes_before, "captured vendor Sandbox/DO starting bytes have an unexplained delta");
  check(expected.vendor_sandbox_do.stored_bytes_after === actual.vendor_sandbox_do.stored_bytes_after, "captured vendor Sandbox/DO ending bytes have an unexplained delta");
  check(JSON.stringify(expected.vendor_sandbox_do.custom_classes) === JSON.stringify(actual.vendor_sandbox_do.custom_classes), "captured custom Durable Object class list has an unexplained delta");
}

function numericPair(value, path, keys) {
  const result = record(value, path);
  for (const key of keys) integer(result[key], `${path}.${key}`);
  return result;
}

function sumNested(runs, parent, key) {
  return runs.reduce((total, run) => total + run[parent][key], 0);
}

function array(value, path) {
  check(Array.isArray(value), `${path} must be an array`);
  return Array.isArray(value) ? value : [];
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(1);
}
