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
const responses = [];
const unsupported = manifest.workload.steps.find((step) => step.id === "unsupported-managed-codex");
const unsupportedResult = await runManagedAgentPreflight(unsupported);
responses.push({ step_id: unsupported.id, ...unsupportedResult });

const first = await call("first-execute", "/v2/executions", request, "EXECUTE");
responses.push(summary("first-execute", first));
expect(first.body, "first-execute", { status: "completed", disposition: "created" });

const retry = await call("exact-retry", "/v2/executions", request, "EXECUTE");
responses.push(summary("exact-retry", retry));
expect(retry.body, "exact-retry", { status: "completed", disposition: "reused" });
if (retry.body.request_hash !== first.body.request_hash) {
  fail("exact retry changed request_hash");
}
if (retry.body.evidence?.artifact_ref?.key !== first.body.evidence?.artifact_ref?.key) {
  fail("exact retry returned a second artifact");
}
if (JSON.stringify(retry.body.cost ?? null) !== JSON.stringify(first.body.cost ?? null)) {
  fail("exact retry returned a different RunCostEnvelope");
}

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
  responses.push(summary(step.id, status));
  expect(status.body, step.id, { status: "completed", disposition: "reused" });
  if (status.body.request_hash !== first.body.request_hash) {
    fail(`${step.id} changed request_hash`);
  }
}

const finalizeStep = manifest.workload.steps.find((step) => step.id === "finalize");
const finalizeRequest = await readJson(finalizeRequestPath, "operator finalize request");
if (finalizeRequest.sessionId !== finalizeStep.session_id) {
  fail("operator finalize request sessionId does not match the fixed workload");
}
const finalized = await call("finalize", "/v2/workspaces/finalize", finalizeRequest, "FINALIZE");
responses.push(summary("finalize", finalized));
if (typeof finalized.body.resultSnapshot !== "string" || !/^[0-9a-f]{64}$/.test(finalized.body.resultSnapshot)) {
  fail("finalize did not return a canonical result snapshot");
}

const report = {
  schema_version: 1,
  deployment: manifest.deployment,
  workload: manifest.workload,
  capture: {
    source: "burnin-fixed-workload",
    responses,
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

function summary(stepId, result) {
  const body = result.body;
  return {
    step_id: stepId,
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
    id: "run-1",
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
  if (!isRecord(value) || value.schema_version !== 1) fail("manifest schema_version must be 1");
  const deployment = value.deployment;
  if (!isRecord(deployment) || deployment.worker_name !== "pillbox-managed-burnin") fail("manifest must target pillbox-managed-burnin");
  if (deployment.managed_execution_limit !== 1 || deployment.reviewed_new_execution_count !== 1) fail("fixed workload allowance must be exactly one new execution");
  if (!isRecord(value.workload) || !Array.isArray(value.workload.steps)) fail("manifest workload.steps must be an array");
  const first = value.workload.steps.find((step) => step?.id === "first-execute");
  if (!isRecord(first) || !isRecord(first.request)) fail("manifest must contain first-execute.request");
  const inputHash = `sha256:${createHash("sha256").update(first.request.rendered_input).digest("hex")}`;
  if (first.request.rendered_input_hash !== inputHash) fail("manifest rendered_input_hash is not deterministic");
  const statusSteps = value.workload.steps.filter((step) => step?.operation === "status");
  for (const step of statusSteps) {
    if (!Number.isSafeInteger(step.evidence_limit) || step.evidence_limit < 1 || step.evidence_limit > MAX_STATUS_PAGE_SIZE) fail(`${step.id} evidence_limit is outside the bounded page size`);
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
