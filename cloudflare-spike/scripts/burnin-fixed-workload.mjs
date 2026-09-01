#!/usr/bin/env node

import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const DEFAULT_MANIFEST = new URL(
  "../testdata/burnin-reconciliation.fixture.json",
  import.meta.url,
);
const MAX_STATUS_PAGE_SIZE = 100;
const REPORT_SCHEMA_VERSION = 2;
const EXPECTED_WORKLOAD = new Map([
  ["first-execute", "execute"],
  ["exact-retry", "execute"],
  ["status-page-1", "status"],
  ["status-page-2", "status"],
  ["unsupported-managed-codex", "managed_codex_preflight"],
  ["finalize", "workspace_finalize"],
]);
const execFileAsync = promisify(execFile);

const args = process.argv.slice(2);
const execute = args.includes("--execute");
const manifestPath = option("--manifest") ?? DEFAULT_MANIFEST;
const recordPath = option("--record");
const baseUrl = option("--base-url") ?? process.env.BURNIN_BASE_URL;
const finalizeRequestPath =
  option("--finalize-request") ?? process.env.BURNIN_FINALIZE_REQUEST_FILE;
const managedPreflightPath =
  option("--managed-preflight") ??
  process.env.BURNIN_MANAGED_PREFLIGHT ??
  fileURLToPath(new URL("../../scripts/smoke/managed-agent-preflight.sh", import.meta.url));

const manifest = await readJson(manifestPath, "burn-in manifest");
validateManifest(manifest);

if (!execute) {
  console.log(JSON.stringify(dryRunPlan(manifest), null, 2));
  process.exit(0);
}

if (process.env.BURNIN_CONFIRM_ISOLATED !== "1") {
  fail("live burn-in requires BURNIN_CONFIRM_ISOLATED=1; verify the endpoint is the isolated preview namespace");
}
if (typeof baseUrl !== "string" || !/^https?:\/\//.test(baseUrl)) {
  fail("live burn-in requires --base-url or BURNIN_BASE_URL with an http(s) isolated preview URL");
}
if (finalizeRequestPath === undefined) {
  fail("live burn-in requires --finalize-request or BURNIN_FINALIZE_REQUEST_FILE; credentials never belong in the repository manifest");
}

const request = manifest.workload.steps.find((step) => step.id === "first-execute").request;
const runtimeCalls = [];
const unsupported = manifest.workload.steps.find((step) => step.id === "unsupported-managed-codex");
const unsupportedResult = await runManagedAgentPreflight(unsupported);
const preflight = {
  step_id: unsupported.id,
  operation: unsupported.operation,
  schema_version: unsupportedResult.schema_version,
  agent: unsupportedResult.agent,
  status: unsupportedResult.status,
  disposition: unsupportedResult.disposition,
  error_code: unsupportedResult.error_code,
  exit_code: unsupportedResult.exit_code,
  observed_output: unsupportedResult.observed_output,
  observed_output_sha256: unsupportedResult.observed_output_sha256,
  counters: unsupportedResult.counters,
};

const first = await call("first-execute", "/v2/executions", request, "EXECUTE");
expect(first.body, "first-execute", { status: "completed", disposition: "created" });
expectTerminalAttribution("first-execute", first, request.invocation_id);
runtimeCalls.push(runtimeSummary("first-execute", "execute", first));

const retry = await call("exact-retry", "/v2/executions", request, "EXECUTE");
expect(retry.body, "exact-retry", { status: "completed", disposition: "reused" });
expectTerminalAttribution("exact-retry", retry, request.invocation_id);
expectSameTerminalExecution("exact-retry", retry.body, first.body);
runtimeCalls.push(runtimeSummary("exact-retry", "execute", retry));

for (const [index, step] of manifest.workload.steps
  .filter((candidate) => candidate.operation === "status")
  .entries()) {
  const statusRequest = {
    contract_version: "pillbox.execution/2",
    invocation_id: request.invocation_id,
    evidence_after: step.evidence_after,
    evidence_limit: step.evidence_limit,
  };
  const status = await call(step.id, "/v2/executions/status", statusRequest, `STATUS_${index + 1}`);
  expect(status.body, step.id, { status: "completed", disposition: "reused" });
  expectTerminalAttribution(step.id, status, request.invocation_id);
  expectSameTerminalExecution(step.id, status.body, first.body);
  runtimeCalls.push(runtimeSummary(step.id, step.operation, status));
}

const finalizeStep = manifest.workload.steps.find((step) => step.id === "finalize");
const finalizeRequest = await readJson(finalizeRequestPath, "operator finalize request");
if (finalizeRequest.sessionId !== finalizeStep.session_id) {
  fail("operator finalize request sessionId does not match the fixed workload");
}
const finalized = await call("finalize", "/v2/workspaces/finalize", finalizeRequest, "FINALIZE");
if (finalized.status !== 200 || typeof finalized.body.resultSnapshot !== "string" || !/^[0-9a-f]{64}$/.test(finalized.body.resultSnapshot)) {
  fail("finalize did not return a canonical result snapshot");
}
const cleanup = {
  step_id: finalizeStep.id,
  operation: finalizeStep.operation,
  http_status: finalized.status,
  session_id: finalizeRequest.sessionId,
  result_snapshot: finalized.body.resultSnapshot,
};

const report = {
  schema_version: REPORT_SCHEMA_VERSION,
  deployment: manifest.deployment,
  workload: manifest.workload,
  capture: {
    source: "burnin-fixed-workload",
    runtime_calls: runtimeCalls,
    preflight,
    cleanup,
    run_cost_envelopes: uniqueRunCost(first.body),
    read_only: null,
    totals: null,
    operator_capture_required: [
      "D1 rows read/written",
      "R2 reads/writes/bytes",
      "Container duration and lifecycle counters",
      "Worker request counters",
      "Analytics Engine point count",
      "vendor Sandbox/DO counters and custom DO storage delta",
    ],
  },
};

if (recordPath !== undefined) {
  await writeFile(resolve(process.cwd(), recordPath), `${JSON.stringify(report, null, 2)}\n`, { mode: 0o600 });
}
console.log(JSON.stringify(report, null, 2));

function dryRunPlan(value) {
  const steps = value.workload.steps.map((step) => ({
    id: step.id,
    operation: step.operation,
    network_requests: networkRequestsForStep(step),
    expected: step.expected ?? {},
  }));
  return {
    mode: "dry-run",
    worker: value.deployment.worker_name,
    container_class: value.deployment.container_class,
    allowance: {
      epoch: value.deployment.allowance_epoch,
      limit: value.deployment.managed_execution_limit,
      reviewed_new_execution_count: value.deployment.reviewed_new_execution_count,
    },
    steps,
    safety: {
      unsupported_managed_codex: "preflight rejected; no request or Sandbox provision",
      exact_retry: "same request hash; no new claim, artifact, or Analytics point",
      status_pages: `bounded at ${MAX_STATUS_PAGE_SIZE} evidence events per request`,
      finalize: "operator-supplied scoped request; kill-before-transfer required",
    },
    next: "Use --execute only with BURNIN_CONFIRM_ISOLATED=1 and operator-captured provider counters, then run npm run burnin:reconcile -- <report.json>.",
  };
}

function networkRequestsForStep(step) {
  if (step.operation === "managed_codex_preflight") return 0;
  if (
    step.id === "first-execute" ||
    step.id === "exact-retry" ||
    step.operation === "status" ||
    step.operation === "workspace_finalize"
  ) {
    return 1;
  }
  return 0;
}

async function call(stepId, path, body, tokenName) {
  const token = process.env[`BURNIN_${tokenName}_TOKEN`];
  if (typeof token !== "string" || token.length === 0) {
    fail(`missing BURNIN_${tokenName}_TOKEN for ${stepId}; capabilities must be minted for the exact request bytes`);
  }
  let response;
  try {
    response = await fetch(new URL(path, ensureTrailingSlash(baseUrl)), {
      method: "POST",
      headers: {
        accept: "application/json",
        "content-type": "application/json",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify(body),
    });
  } catch (error) {
    fail(`${stepId} request failed before a response: ${String(error)}`);
  }
  let responseBody;
  try {
    responseBody = await response.json();
  } catch {
    fail(`${stepId} returned non-JSON HTTP ${response.status}`);
  }
  if (!response.ok) {
    const code = responseBody?.error?.code ?? "unknown";
    fail(`${stepId} returned HTTP ${response.status} (${code})`);
  }
  return { status: response.status, body: responseBody };
}

async function runManagedAgentPreflight(step) {
  let stdout;
  try {
    ({ stdout } = await execFileAsync(managedPreflightPath, [], {
      cwd: resolve(fileURLToPath(new URL("../..", import.meta.url))),
      env: process.env,
      encoding: "utf8",
      timeout: 120_000,
      maxBuffer: 64 * 1024,
    }));
  } catch (error) {
    fail(`${step.id} executable preflight failed: ${error.stderr || error.message}`);
  }
  let observed;
  try {
    observed = JSON.parse(stdout);
  } catch {
    fail(`${step.id} executable preflight returned non-JSON output`);
  }
  if (
    observed?.schema_version !== 1 ||
    observed?.agent !== "codex" ||
    observed?.status !== "preflight_rejected" ||
    observed?.disposition !== "not_sent" ||
    observed?.error_code !== "unsupported_execution" ||
    observed?.exit_code !== 2 ||
    typeof observed?.observed_output !== "string" ||
    observed?.observed_output_sha256 !== digestText(observed.observed_output) ||
    observed?.counters?.provision_attempts !== 0 ||
    observed?.counters?.network_requests !== 0 ||
    observed?.counters?.state_entries_created !== 0
  ) {
    fail(`${step.id} did not observe the fail-closed managed boundary`);
  }
  return observed;
}

function runtimeSummary(stepId, operation, result) {
  const body = result.body;
  return {
    step_id: stepId,
    operation,
    http_status: result.status,
    invocation_id: body.invocation_id,
    status: body.status,
    disposition: body.disposition,
    request_hash: body.request_hash,
    artifact_key: body.evidence?.artifact_ref?.key ?? null,
    cost_ref: body.cost === undefined ? null : digestOf(body.cost),
  };
}

function uniqueRunCost(body) {
  if (body.cost === undefined) return [];
  return [{
    id: digestOf(body.cost),
    invocation_id: body.invocation_id,
    artifact_count: body.evidence?.artifact_ref === undefined ? 0 : 1,
    analytics_point_count: body.cost.infrastructure.analytics_points_written,
    cost: body.cost,
    observed: null,
  }];
}

function expect(body, stepId, expected) {
  for (const [key, value] of Object.entries(expected)) {
    if (body?.[key] !== value) fail(`${stepId} expected ${key}=${value}`);
  }
}

function validateManifest(value) {
  if (!isRecord(value) || value.schema_version !== REPORT_SCHEMA_VERSION) fail(`manifest schema_version must be ${REPORT_SCHEMA_VERSION}`);
  const deployment = value.deployment;
  if (!isRecord(deployment) || deployment.worker_name !== "pillbox-managed-burnin") fail("manifest must target pillbox-managed-burnin");
  if (deployment.managed_execution_limit !== 1 || deployment.reviewed_new_execution_count !== 1 || deployment.reserved_executions !== 1) fail("fixed workload allowance must be exactly one reserved execution");
  if (!isRecord(value.workload) || !Array.isArray(value.workload.steps)) fail("manifest workload.steps must be an array");
  const stepById = new Map();
  for (const step of value.workload.steps) {
    if (!isRecord(step) || typeof step.id !== "string" || stepById.has(step.id)) fail("manifest workload steps must have unique string ids");
    stepById.set(step.id, step);
  }
  for (const [stepId, operation] of EXPECTED_WORKLOAD) {
    if (stepById.get(stepId)?.operation !== operation) fail(`manifest step ${stepId} must use operation ${operation}`);
  }
  for (const stepId of stepById.keys()) {
    if (!EXPECTED_WORKLOAD.has(stepId)) fail(`manifest contains unexpected workload step ${stepId}`);
  }
  if (stepById.size !== EXPECTED_WORKLOAD.size) fail("manifest must contain exactly the checked burn-in steps");
  const first = stepById.get("first-execute");
  if (!isRecord(first) || !isRecord(first.request)) fail("manifest must contain first-execute.request");
  if (first.invocation_id !== first.request.invocation_id) fail("first-execute invocation identity must match its request");
  if (stepById.get("exact-retry")?.request_ref !== "first-execute") fail("exact-retry must reference first-execute");
  if (first.expected?.allowance_reservation !== 1) fail("first-execute must reserve the one reviewed allowance");
  if (stepById.get("exact-retry")?.expected?.allowance_reservation_delta !== 0 || stepById.get("exact-retry")?.expected?.new_model_turns !== 0) fail("exact-retry must not reserve or sample again");
  const inputHash = `sha256:${createHash("sha256").update(first.request.rendered_input).digest("hex")}`;
  if (first.request.rendered_input_hash !== inputHash) fail("manifest rendered_input_hash is not deterministic");
  const statusSteps = value.workload.steps.filter((step) => step?.operation === "status");
  for (const [index, step] of statusSteps.entries()) {
    if (step.evidence_limit !== MAX_STATUS_PAGE_SIZE || step.evidence_after !== index * MAX_STATUS_PAGE_SIZE || step.expected?.bounded !== true) fail(`${step.id} must request the checked bounded evidence page`);
  }
  const networkSteps = value.workload.steps.filter((step) => step.operation !== "managed_codex_preflight");
  if (value.workload.expected_network_requests !== networkSteps.length) fail("manifest network request count must match runtime and cleanup calls");
  const unsupported = stepById.get("unsupported-managed-codex");
  if (unsupported.expected?.error_code !== "unsupported_execution" || unsupported.expected?.provision_attempts !== 0 || unsupported.expected?.network_requests !== 0) fail("managed Codex preflight must reject without side effects");
  const finalize = stepById.get("finalize");
  if (finalize.requires_operator_request !== true || finalize.expected?.requests !== 1 || finalize.expected?.kill_before_transfer !== true || finalize.expected?.result_snapshot_required !== true) fail("finalize must be one operator-scoped kill-before-transfer cleanup");
}

function expectSameTerminalExecution(stepId, body, firstBody) {
  if (body.request_hash !== firstBody.request_hash) {
    fail(`${stepId} changed request_hash`);
  }
  if (body.evidence?.artifact_ref?.key !== firstBody.evidence?.artifact_ref?.key) {
    fail(`${stepId} returned a second artifact`);
  }
  if (JSON.stringify(body.cost ?? null) !== JSON.stringify(firstBody.cost ?? null)) {
    fail(`${stepId} returned a different RunCostEnvelope`);
  }
}

function expectTerminalAttribution(stepId, result, invocationId) {
  const body = result.body;
  if (
    result.status !== 200 ||
    body.invocation_id !== invocationId ||
    typeof body.request_hash !== "string" ||
    !/^sha256:[0-9a-f]{64}$/.test(body.request_hash) ||
    typeof body.evidence?.artifact_ref?.key !== "string" ||
    body.evidence.artifact_ref.key.length === 0 ||
    !isRecord(body.cost) ||
    !isRecord(body.cost.infrastructure)
  ) {
    fail(`${stepId} omitted required terminal invocation attribution`);
  }
}

async function readJson(path, label) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    fail(`cannot read ${label} ${String(path)}: ${String(error)}`);
  }
}

function option(name) {
  const index = args.indexOf(name);
  return index < 0 ? undefined : args[index + 1];
}

function ensureTrailingSlash(value) {
  return value.endsWith("/") ? value : `${value}/`;
}

function digestOf(value) {
  return `sha256:${createHash("sha256").update(JSON.stringify(value)).digest("hex")}`;
}

function digestText(value) {
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(1);
}
