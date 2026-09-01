#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

const DEFAULT_FIXTURE = new URL(
  "../testdata/burnin-reconciliation.fixture.json",
  import.meta.url,
);
const MAX_STATUS_PAGE_SIZE = 100;
const REPORT_SCHEMA_VERSION = 3;
const EXPECTED_WORKLOAD = new Map([
  ["first-execute", "execute"],
  ["exact-retry", "execute"],
  ["status-page-1", "status"],
  ["status-page-2", "status"],
  ["unsupported-managed-codex", "managed_codex_preflight"],
  ["finalize", "workspace_finalize"],
]);
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
check(root.schema_version === REPORT_SCHEMA_VERSION, `fixture.schema_version must be ${REPORT_SCHEMA_VERSION}`);
const deployment = record(root.deployment, "fixture.deployment");
const workload = record(root.workload, "fixture.workload");
const capture = record(root.capture, "fixture.capture");
allowedKeys(capture, [
  "source",
  "notes",
  "execution_identity",
  "runtime_calls",
  "preflight",
  "cleanup",
  "run_cost_envelopes",
  "read_only",
  "totals",
  "operator_capture_required",
], "fixture.capture");
for (const key of ["source", "execution_identity", "runtime_calls", "preflight", "cleanup", "run_cost_envelopes", "read_only", "totals"]) {
  check(Object.hasOwn(capture, key), `fixture.capture.${key} is required by the burn-in report contract`);
}
string(capture.source, "fixture.capture.source");

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
for (const [stepId, operation] of EXPECTED_WORKLOAD) {
  check(stepById.get(stepId)?.operation === operation, `workload step ${stepId} must use operation ${operation}`);
}
for (const stepId of stepById.keys()) {
  check(EXPECTED_WORKLOAD.has(stepId), `workload contains unexpected step ${stepId}`);
}
check(stepById.size === EXPECTED_WORKLOAD.size, "workload must contain exactly the checked burn-in steps");

const runtimeSteps = steps.filter((step) => step?.operation === "execute" || step?.operation === "status");
const preflightSteps = steps.filter((step) => step?.operation === "managed_codex_preflight");
const cleanupSteps = steps.filter((step) => step?.operation === "workspace_finalize");
check(
  runtimeSteps.length + preflightSteps.length + cleanupSteps.length === steps.length,
  "workload contains an operation outside the checked burn-in report contract",
);

const first = stepById.get("first-execute");
const retry = stepById.get("exact-retry");
const statusSteps = [stepById.get("status-page-1"), stepById.get("status-page-2")];
const codex = stepById.get("unsupported-managed-codex");
const finalize = stepById.get("finalize");
check(first?.operation === "execute", "workload must start with first-execute");
check(retry?.operation === "execute" && retry?.request_ref === "first-execute", "exact-retry must reuse the first execute request");
check(first?.expected?.allowance_reservation === 1, "first-execute must reserve the one reviewed allowance");
check(retry?.expected?.allowance_reservation_delta === 0, "exact-retry must not reserve another allowance");
check(retry?.expected?.new_model_turns === 0, "exact-retry must not sample another model turn");
for (const [index, step] of statusSteps.entries()) {
  const path = `status-page-${index + 1}`;
  check(step?.operation === "status", `${path} must be a status read`);
  const limit = integer(step?.evidence_limit, `${path}.evidence_limit`);
  check(limit === MAX_STATUS_PAGE_SIZE, `${path}.evidence_limit must be ${MAX_STATUS_PAGE_SIZE}`);
  check(integer(step?.evidence_after, `${path}.evidence_after`) === index * MAX_STATUS_PAGE_SIZE, `${path}.evidence_after must select the checked page`);
  check(step?.expected?.bounded === true, `${path} must declare bounded evidence`);
}
check(codex?.operation === "managed_codex_preflight", "unsupported-managed-codex must be an explicit preflight step");
check(record(codex?.expected, "unsupported-managed-codex.expected").error_code === "unsupported_execution", "managed Codex preflight must fail with unsupported_execution");
check(integer(codex?.expected?.provision_attempts, "unsupported-managed-codex.expected.provision_attempts") === 0, "unsupported managed Codex must not provision a container");
check(integer(codex?.expected?.network_requests, "unsupported-managed-codex.expected.network_requests") === 0, "unsupported managed Codex preflight must make no network request");
check(finalize?.operation === "workspace_finalize", "workload must include workspace finalize");
check(finalize?.requires_operator_request === true, "finalize must require an operator-supplied scoped request");
check(record(finalize?.expected, "finalize.expected").kill_before_transfer === true, "finalize must quiesce the container before transfer credentials enter");
check(finalize?.expected?.requests === 1, "finalize must make exactly one cleanup request");
check(finalize?.expected?.result_snapshot_required === true, "finalize must require a result snapshot");

const expectedNetworkRequests = integer(workload.expected_network_requests, "workload.expected_network_requests");
check(
  expectedNetworkRequests === runtimeSteps.length + cleanupSteps.length,
  "workload network request count has an unexplained operation",
);

let derivedIdentity = {};
if (isRecord(first?.request)) {
  const request = first.request;
  const input = string(request.rendered_input, "first-execute.request.rendered_input");
  const expectedInputHash = `sha256:${createHash("sha256").update(input).digest("hex")}`;
  check(first.invocation_id === request.invocation_id, "first-execute invocation identity must match its request");
  check(request.rendered_input_hash === expectedInputHash, "first-execute rendered_input_hash does not match the sealed input");
  check(request.tool_policy === "deny_all", "fixed burn-in must deny managed runtime tools");
  check(request.execution?.placement === "managed_container", "first-execute must target managed_container");
  check(request.execution?.transport?.harness === "opencode", "first-execute must use the supported managed OpenCode harness");
  derivedIdentity = requestIdentity(request);
}
const capturedIdentity = executionIdentity(capture.execution_identity, "capture.execution_identity");
check(canonicalJson(capturedIdentity) === canonicalJson(derivedIdentity), "capture.execution_identity does not match the canonical first-execute request");
const expectedArtifactKey = `executions/${createHash("sha256").update(capturedIdentity.invocation_id).digest("hex")}/${capturedIdentity.request_hash.slice("sha256:".length)}.json`;

const runtimeCalls = array(capture.runtime_calls, "capture.runtime_calls").map((value, index) =>
  record(value, `capture.runtime_calls[${index}]`)
);
const runtimeCallByStep = indexedObservations(runtimeCalls, "capture.runtime_calls");
checkExactStepCoverage(runtimeSteps, runtimeCallByStep, "capture.runtime_calls");
for (const [index, response] of runtimeCalls.entries()) {
  const path = `capture.runtime_calls[${index}]`;
  exactKeys(response, [
    "step_id",
    "operation",
    "http_status",
    "invocation_id",
    "status",
    "disposition",
    "request_hash",
    "execution_digest",
    "execution_policy_revision",
    "session_ref",
    "evidence",
    "cost_ref",
  ], path);
  check(response.operation === stepById.get(response.step_id)?.operation, `${path}.operation does not match the workload step`);
  check(integer(response.http_status, `${path}.http_status`) === 200, `${path}.http_status must be 200`);
  check(response.invocation_id === first?.invocation_id, `${path}.invocation_id does not identify the one reviewed invocation`);
  check(response.request_hash === capturedIdentity.request_hash, `${path}.request_hash does not match the sealed request`);
  check(response.execution_digest === capturedIdentity.execution_digest, `${path}.execution_digest does not match the sealed execution`);
  check(response.execution_policy_revision === capturedIdentity.execution_policy_revision, `${path}.execution_policy_revision changed`);
  const session = positionalSessionRef(response.session_ref, `${path}.session_ref`);
  check(session.session_id === capturedIdentity.session_id, `${path}.session_ref identifies another session`);
  const page = evidencePage(response.evidence, `${path}.evidence`);
  const step = stepById.get(response.step_id);
  const expectedFrom = step?.operation === "status" ? step.evidence_after : 0;
  check(page.from === expectedFrom, `${path}.evidence.from breaks positional continuity`);
  digest(response.cost_ref, `${path}.cost_ref`);
}
const firstResponse = runtimeCallByStep.get("first-execute");
check(firstResponse?.disposition === "created", "first execute must create the only execution claim");
check(firstResponse?.status === "completed", "first execute fixture must complete");
const terminalRange = positionalSessionRef(firstResponse?.session_ref, "first-execute.session_ref").seq_range;
for (const response of runtimeCalls) {
  validateEvidenceContinuity(response.evidence, terminalRange, `capture.runtime_calls.${response.step_id}.evidence`);
}
for (const stepId of ["exact-retry", "status-page-1", "status-page-2"]) {
  const response = runtimeCallByStep.get(stepId);
  check(response?.disposition === "reused", `${stepId} must reuse the first terminal result`);
  check(response?.status === "completed", `${stepId} must observe the completed terminal result`);
  for (const field of ["request_hash", "execution_digest", "execution_policy_revision"]) {
    check(response?.[field] === firstResponse?.[field], `${stepId} changed ${field} during replay`);
  }
  check(canonicalJson(response?.session_ref) === canonicalJson(firstResponse?.session_ref), `${stepId} changed the positional session range`);
  check(canonicalJson(response?.evidence?.artifact_ref) === canonicalJson(firstResponse?.evidence?.artifact_ref), `${stepId} used a second artifact identity or digest`);
  check(response?.cost_ref === firstResponse?.cost_ref, `${stepId} used a second cost envelope`);
}

check(preflightSteps.length === 1, "workload must contain exactly one managed preflight step");
const preflight = record(capture.preflight, "capture.preflight");
exactKeys(preflight, [
  "step_id",
  "operation",
  "schema_version",
  "agent",
  "status",
  "disposition",
  "error_code",
  "exit_code",
  "observed_output",
  "observed_output_sha256",
  "counters",
], "capture.preflight");
check(preflight.step_id === preflightSteps[0]?.id, "capture.preflight does not match the workload preflight step");
check(preflight.operation === preflightSteps[0]?.operation, "capture.preflight.operation does not match the workload step");
check(preflight.schema_version === 1, "capture.preflight.schema_version must be 1");
check(preflight.agent === "codex", "capture.preflight.agent must be codex");
check(preflight.status === "preflight_rejected", "capture.preflight.status must be preflight_rejected");
check(preflight.disposition === "not_sent", "capture.preflight.disposition must be not_sent");
check(preflight.error_code === "unsupported_execution", "capture.preflight.error_code must be unsupported_execution");
check(preflight.exit_code === 2, "capture.preflight.exit_code must be 2");
const observedOutput = string(preflight.observed_output, "capture.preflight.observed_output");
check(preflight.observed_output_sha256 === digestText(observedOutput), "capture.preflight.observed_output_sha256 does not match observed output");
const preflightCounters = record(preflight.counters, "capture.preflight.counters");
exactKeys(preflightCounters, ["provision_attempts", "network_requests", "state_entries_created"], "capture.preflight.counters");
for (const key of ["provision_attempts", "network_requests", "state_entries_created"]) {
  check(integer(preflightCounters[key], `capture.preflight.counters.${key}`) === 0, `capture.preflight.counters.${key} must be zero`);
}

check(cleanupSteps.length === 1, "workload must contain exactly one workspace cleanup step");
const cleanup = record(capture.cleanup, "capture.cleanup");
exactKeys(cleanup, ["step_id", "operation", "http_status", "session_id", "result_snapshot"], "capture.cleanup");
check(cleanup.step_id === cleanupSteps[0]?.id, "capture.cleanup does not match the workload cleanup step");
check(cleanup.operation === cleanupSteps[0]?.operation, "capture.cleanup.operation does not match the workload step");
check(integer(cleanup.http_status, "capture.cleanup.http_status") === 200, "capture.cleanup.http_status must be 200");
check(cleanup.session_id === cleanupSteps[0]?.session_id, "capture.cleanup.session_id does not match the finalized workspace");
check(typeof cleanup.result_snapshot === "string" && /^[0-9a-f]{64}$/.test(cleanup.result_snapshot), "capture.cleanup.result_snapshot must be a canonical snapshot handle");

const runs = array(capture.run_cost_envelopes, "capture.run_cost_envelopes");
check(runs.length === reviewedCount, "there must be exactly one captured RunCostEnvelope per genuinely new execution");
const runIds = new Set();
const observedRuns = [];
for (const [index, rawRun] of runs.entries()) {
  const run = record(rawRun, `capture.run_cost_envelopes[${index}]`);
  exactKeys(run, [
    "id",
    "execution_identity",
    "artifact_ref",
    "artifact_count",
    "analytics_point_count",
    "cost",
    "observed",
  ], `capture.run_cost_envelopes[${index}]`);
  const runId = string(run.id, `capture.run_cost_envelopes[${index}].id`);
  const runIdentity = executionIdentity(run.execution_identity, `capture.run_cost_envelopes[${index}].execution_identity`);
  const runArtifact = artifactRef(run.artifact_ref, `capture.run_cost_envelopes[${index}].artifact_ref`);
  check(!runIds.has(runId), `duplicate RunCostEnvelope id ${runId}`);
  runIds.add(runId);
  check(canonicalJson(runIdentity) === canonicalJson(capturedIdentity), `${runId} is not attributed to the full first-execute identity`);
  check(canonicalJson(runArtifact) === canonicalJson(firstResponse?.evidence?.artifact_ref), `${runId} does not seal the terminal artifact identity and digest`);
  check(runArtifact.key === expectedArtifactKey, `${runId} artifact key does not derive from invocation_id and request_hash`);
  const cost = validateCost(run.cost, `${runId}.cost`);
  check(runId === digestOf(cost), `${runId}.id must be the RunCostEnvelope digest`);
  check(firstResponse?.cost_ref === runId, `${runId} is not referenced by first-execute`);
  const observed = record(run.observed, `${runId}.observed`);
  const d1 = record(observed.d1, `${runId}.observed.d1`);
  const r2 = record(observed.r2, `${runId}.observed.r2`);
  const container = record(observed.container, `${runId}.observed.container`);
  const worker = record(observed.worker, `${runId}.observed.worker`);
  const analytics = record(observed.analytics_engine, `${runId}.observed.analytics_engine`);
  const vendor = validateVendorCounters(observed.vendor_sandbox_do, `${runId}.observed.vendor_sandbox_do`);
  const infra = record(cost.infrastructure, `${runId}.cost.infrastructure`);
  check(runArtifact.bytes === integer(infra.r2_bytes_written, `${runId}.cost.infrastructure.r2_bytes_written`), `${runId} artifact byte identity disagrees with its RunCostEnvelope`);
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

function indexedObservations(observations, path) {
  const byStep = new Map();
  for (const [index, observation] of observations.entries()) {
    const stepId = string(observation.step_id, `${path}[${index}].step_id`);
    check(!byStep.has(stepId), `${path} contains duplicate step ${stepId}`);
    byStep.set(stepId, observation);
  }
  return byStep;
}

function checkExactStepCoverage(expectedSteps, observations, path) {
  const expectedIds = new Set(expectedSteps.map((step) => step.id));
  for (const stepId of expectedIds) {
    check(observations.has(stepId), `${path} omitted workload step ${stepId}`);
  }
  for (const stepId of observations.keys()) {
    check(expectedIds.has(stepId), `${path} contains unexpected workload step ${stepId}`);
  }
  check(observations.size === expectedIds.size, `${path} must contain exactly one observation per runtime workload step`);
}

function exactKeys(value, keys, path) {
  allowedKeys(value, keys, path);
  for (const key of keys) {
    check(Object.hasOwn(value, key), `${path}.${key} is required by the burn-in report contract`);
  }
}

function allowedKeys(value, keys, path) {
  const allowed = new Set(keys);
  for (const key of Object.keys(value)) {
    check(allowed.has(key), `${path}.${key} is not allowed by the burn-in report contract`);
  }
}

function digestOf(value) {
  return digestText(canonicalJson(value));
}

function requestIdentity(request) {
  return {
    invocation_id: request.invocation_id,
    idempotency_key: request.idempotency_key,
    session_id: request.session_ref.session_id,
    request_hash: digestOf(request),
    execution_digest: digestOf({
      execution: request.execution,
      execution_policy_revision: request.execution_policy_revision,
    }),
    execution_policy_revision: request.execution_policy_revision,
  };
}

function executionIdentity(value, path) {
  const identity = record(value, path);
  exactKeys(identity, [
    "invocation_id",
    "idempotency_key",
    "session_id",
    "request_hash",
    "execution_digest",
    "execution_policy_revision",
  ], path);
  for (const key of ["invocation_id", "idempotency_key", "session_id", "execution_policy_revision"]) {
    string(identity[key], `${path}.${key}`);
  }
  digest(identity.request_hash, `${path}.request_hash`);
  digest(identity.execution_digest, `${path}.execution_digest`);
  check(identity.idempotency_key === identity.invocation_id, `${path}.idempotency_key must equal invocation_id`);
  return identity;
}

function positionalSessionRef(value, path) {
  const session = record(value, path);
  exactKeys(session, ["session_id", "seq_range"], path);
  string(session.session_id, `${path}.session_id`);
  const range = array(session.seq_range, `${path}.seq_range`);
  check(range.length === 2, `${path}.seq_range must contain exactly two positions`);
  const start = integer(range[0], `${path}.seq_range[0]`);
  const end = integer(range[1], `${path}.seq_range[1]`);
  check(start === 0 && end >= start, `${path}.seq_range must be an inclusive zero-based artifact range`);
  return { ...session, seq_range: [start, end] };
}

function artifactRef(value, path) {
  const artifact = record(value, path);
  exactKeys(artifact, ["key", "media_type", "bytes", "sha256"], path);
  string(artifact.key, `${path}.key`);
  check(artifact.media_type === "application/json", `${path}.media_type must be application/json`);
  check(integer(artifact.bytes, `${path}.bytes`) > 0, `${path}.bytes must be positive`);
  digest(artifact.sha256, `${path}.sha256`);
  return artifact;
}

function evidencePage(value, path) {
  const page = record(value, path);
  exactKeys(page, ["from", "next", "truncated", "event_count", "artifact_ref"], path);
  const from = integer(page.from, `${path}.from`);
  const eventCount = integer(page.event_count, `${path}.event_count`);
  check(eventCount <= MAX_STATUS_PAGE_SIZE, `${path}.event_count exceeds the bounded page size`);
  if (page.next !== null) integer(page.next, `${path}.next`);
  check(typeof page.truncated === "boolean", `${path}.truncated must be boolean`);
  check(page.truncated === (page.next !== null), `${path}.truncated disagrees with next`);
  if (page.next !== null) check(page.next === from + eventCount, `${path}.next breaks positional continuity`);
  return { ...page, from, event_count: eventCount, artifact_ref: artifactRef(page.artifact_ref, `${path}.artifact_ref`) };
}

function validateEvidenceContinuity(value, seqRange, path) {
  const page = evidencePage(value, path);
  const end = seqRange[1];
  if (page.from > end) {
    check(page.event_count === 0 && page.next === null, `${path} reads positions beyond the artifact range`);
  } else if (page.next === null) {
    check(page.event_count === end - page.from + 1, `${path} omits terminal artifact positions`);
  } else {
    check(page.next <= end, `${path}.next exceeds the terminal artifact range`);
  }
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort((left, right) => left < right ? -1 : left > right ? 1 : 0)
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

function digestText(value) {
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
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
