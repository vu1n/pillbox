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
const RECONCILER = fileURLToPath(
  new URL("./reconcile-burnin.mjs", import.meta.url),
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
const execFileAsync = promisify(execFile);

const args = process.argv.slice(2);
const preflightMode = args.includes("--preflight");
const finalizeMode = args.includes("--finalize");
const manifestPath = option("--manifest") ?? DEFAULT_MANIFEST;
const recordPath = option("--record");
const reportPath = option("--report") ?? process.env.BURNIN_HUDDLES_REPORT_FILE;
const baseUrl = option("--base-url") ?? process.env.BURNIN_BASE_URL;
const finalizeRequestPath =
  option("--finalize-request") ?? process.env.BURNIN_FINALIZE_REQUEST_FILE;
const pillboxName = option("--pillbox") ?? process.env.BURNIN_PILLBOX_NAME;
const managedPreflightPath =
  option("--managed-preflight") ??
  process.env.BURNIN_MANAGED_PREFLIGHT ??
  fileURLToPath(
    new URL("../../scripts/smoke/managed-agent-preflight.sh", import.meta.url),
  );

const manifest = await readJson(manifestPath, "burn-in manifest");
validateManifest(manifest);

if (!preflightMode && !finalizeMode) {
  if (args.includes("--execute")) {
    fail(
      "the combined Pillbox recorder is retired; choose --preflight or --finalize",
    );
  }
  console.log(JSON.stringify(dryRunPlan(manifest), null, 2));
  process.exit(0);
}
if (preflightMode === finalizeMode)
  fail("choose exactly one of --preflight or --finalize");

const unsupported = manifest.workload.steps.find(
  (step) => step.id === "unsupported-managed-codex",
);
if (preflightMode) {
  const observed = await runManagedAgentPreflight(unsupported);
  const attachment = {
    schema_version: REPORT_SCHEMA_VERSION,
    deployment: manifest.deployment,
    workload: manifest.workload,
    capture: {
      source: "pillbox-managed-preflight",
      preflight: preflightSummary(unsupported, observed),
    },
  };
  await writeReport(recordPath, attachment);
  console.log(JSON.stringify(attachment, null, 2));
  process.exit(0);
}

if (process.env.BURNIN_CONFIRM_ISOLATED !== "1") {
  fail(
    "live finalize requires BURNIN_CONFIRM_ISOLATED=1; verify the endpoint is the isolated preview namespace",
  );
}
if (typeof baseUrl !== "string" || !/^https?:\/\//.test(baseUrl)) {
  fail(
    "live finalize requires --base-url or BURNIN_BASE_URL with an http(s) isolated preview URL",
  );
}
if (finalizeRequestPath === undefined) {
  fail(
    "live finalize requires --finalize-request or BURNIN_FINALIZE_REQUEST_FILE; credentials never belong in the report",
  );
}
if (reportPath === undefined) {
  fail(
    "live finalize requires --report or BURNIN_HUDDLES_REPORT_FILE for the authoritative Huddles partial",
  );
}
if (typeof pillboxName !== "string" || pillboxName.length === 0) {
  fail(
    "live finalize requires --pillbox or BURNIN_PILLBOX_NAME for rustic snapshot verification",
  );
}
if (
  recordPath !== undefined &&
  resolve(process.cwd(), recordPath) !== resolve(process.cwd(), reportPath)
) {
  fail("finalize must attach cleanup to the same authoritative Huddles report");
}

const report = await readJson(reportPath, "authoritative Huddles report");
if (
  report?.schema_version !== REPORT_SCHEMA_VERSION ||
  report?.capture?.source !== "huddles-managed-burnin" ||
  report.capture.cleanup !== null ||
  canonicalJson(report.deployment) !== canonicalJson(manifest.deployment) ||
  canonicalJson(report.workload) !== canonicalJson(manifest.workload)
) {
  fail(
    "finalize requires the matching Huddles report-v3 partial with cleanup still pending",
  );
}
// Terminal validation precedes the only public call so cleanup cannot predate Huddles evidence.
await validateHuddlesPartial(reportPath);

const finalizeStep = report.workload.steps.find(
  (step) => step.id === "finalize",
);
const finalizeRequest = await readJson(
  finalizeRequestPath,
  "operator finalize request",
);
if (finalizeRequest.sessionId !== finalizeStep.session_id) {
  fail(
    "operator finalize request sessionId does not match the terminal Huddles execution",
  );
}
const requestedBaseSnapshot = finalizeRequest?.workspace?.snapshot;
if (
  typeof requestedBaseSnapshot !== "string" ||
  !/^[0-9a-f]{64}$/.test(requestedBaseSnapshot)
) {
  fail(
    "operator finalize request workspace.snapshot must be a canonical base snapshot handle",
  );
}
const expectedRequestDigest = digestText(canonicalJson(finalizeRequest));
const expectedFinalizeId = digestText(
  canonicalJson({
    session_id: finalizeRequest.sessionId,
    request_digest: expectedRequestDigest,
  }),
);
const finalized = await callFinalize(finalizeRequest);
if (
  finalized.status !== 200 ||
  finalized.body.status !== "completed" ||
  !["created", "reused"].includes(finalized.body.disposition) ||
  typeof finalized.body.finalizeId !== "string" ||
  !/^sha256:[0-9a-f]{64}$/.test(finalized.body.finalizeId) ||
  typeof finalized.body.requestDigest !== "string" ||
  !/^sha256:[0-9a-f]{64}$/.test(finalized.body.requestDigest) ||
  finalized.body.requestDigest !== expectedRequestDigest ||
  finalized.body.finalizeId !== expectedFinalizeId ||
  typeof finalized.body.resultSnapshot !== "string" ||
  !/^[0-9a-f]{64}$/.test(finalized.body.resultSnapshot)
) {
  fail(
    "finalize/status did not return the canonical request digest, finalize ID, and result snapshot",
  );
}
await verifySnapshotAncestry({
  resultSnapshot: finalized.body.resultSnapshot,
  requestedBaseSnapshot,
});
const completed = {
  ...report,
  capture: {
    ...report.capture,
    cleanup: {
      step_id: finalizeStep.id,
      operation: finalizeStep.operation,
      http_status: finalized.status,
      request_count: finalized.requestCount,
      disposition: finalized.body.disposition,
      finalize_id: finalized.body.finalizeId,
      request_digest: finalized.body.requestDigest,
      session_id: finalizeRequest.sessionId,
      requested_base_snapshot: requestedBaseSnapshot,
      result_snapshot: finalized.body.resultSnapshot,
    },
  },
};
await writeFile(
  resolve(process.cwd(), reportPath),
  `${JSON.stringify(completed, null, 2)}\n`,
  {
    mode: 0o600,
  },
);
console.log(JSON.stringify(completed, null, 2));

function dryRunPlan(value) {
  return {
    mode: "dry-run",
    worker: value.deployment.worker_name,
    container_class: value.deployment.container_class,
    allowance: {
      epoch: value.deployment.allowance_epoch,
      limit: value.deployment.managed_execution_limit,
      reviewed_new_execution_count:
        value.deployment.reviewed_new_execution_count,
    },
    accounting: {
      analytics_points_planned: value.deployment.reviewed_new_execution_count,
      analytics_points_observed: "provider capture required after the run",
      analytics_variance: "observed minus planned",
    },
    sequence: [
      "Pillbox --preflight records the real local managed-Codex rejection without HTTP execution or finalize",
      "Huddles validates the unconsumed allowance, records the only created execute, exact retry, and two bounded private status reads, then validates the consumed allowance",
      "Pillbox --finalize validates the Huddles report-v3 partial and rustic ancestry, then attaches scoped public cleanup to that same file",
    ],
    safety: {
      unsupported_managed_codex:
        "preflight rejected; no request or Sandbox provision",
      exact_retry:
        "same request hash; no new claim, artifact, or Analytics point",
      status_pages: `bounded at ${MAX_STATUS_PAGE_SIZE} evidence events per request`,
      execution: "private Huddles service binding only",
      finalize:
        "one public operator-scoped request after terminal positional evidence",
    },
    next: "Run --preflight, then the Huddles recorder, then --finalize against that Huddles report file.",
  };
}

function preflightSummary(step, observed) {
  return {
    step_id: step.id,
    operation: step.operation,
    schema_version: observed.schema_version,
    agent: observed.agent,
    status: observed.status,
    disposition: observed.disposition,
    error_code: observed.error_code,
    exit_code: observed.exit_code,
    observed_output: observed.observed_output,
    observed_output_sha256: observed.observed_output_sha256,
    counters: observed.counters,
  };
}

async function validateHuddlesPartial(path) {
  try {
    await execFileAsync(
      process.execPath,
      [RECONCILER, resolve(process.cwd(), path), "--partial"],
      {
        encoding: "utf8",
        maxBuffer: 64 * 1024,
      },
    );
  } catch (error) {
    fail(
      `authoritative Huddles report-v3 partial failed validation: ${error.stderr || error.message}`,
    );
  }
}

async function writeReport(path, value) {
  if (path === undefined) return;
  await writeFile(
    resolve(process.cwd(), path),
    `${JSON.stringify(value, null, 2)}\n`,
    {
      mode: 0o600,
    },
  );
}

async function callFinalize(body) {
  const token = process.env.BURNIN_FINALIZE_TOKEN;
  if (typeof token !== "string" || token.length === 0) {
    fail(
      "missing BURNIN_FINALIZE_TOKEN; capability must be minted for the exact finalize request bytes",
    );
  }
  const encodedBody = JSON.stringify(body);
  let response;
  let requestCount = 1;
  try {
    response = await fetch(
      new URL("/v2/workspaces/finalize", ensureTrailingSlash(baseUrl)),
      {
        method: "POST",
        headers: {
          accept: "application/json",
          "content-type": "application/json",
          authorization: `Bearer ${token}`,
        },
        body: encodedBody,
      },
    );
  } catch (error) {
    requestCount += 1;
    try {
      response = await fetch(
        new URL("/v2/workspaces/finalize/status", ensureTrailingSlash(baseUrl)),
        {
          method: "POST",
          headers: {
            accept: "application/json",
            "content-type": "application/json",
            authorization: `Bearer ${token}`,
          },
          body: encodedBody,
        },
      );
    } catch (statusError) {
      fail(
        `finalize response was lost and durable status failed: ${String(statusError)}`,
      );
    }
  }
  let responseBody;
  try {
    responseBody = await response.json();
  } catch {
    fail(`finalize/status returned non-JSON HTTP ${response.status}`);
  }
  if (!response.ok) {
    const code = responseBody?.error?.code ?? "unknown";
    fail(`finalize/status returned HTTP ${response.status} (${code})`);
  }
  if (response.status === 202)
    fail("finalize response was lost and durable status remains running");
  return { status: response.status, body: responseBody, requestCount };
}

async function verifySnapshotAncestry({
  resultSnapshot,
  requestedBaseSnapshot,
}) {
  // The Worker response names a handle; only the rustic store can prove its DAG parentage.
  let stdout;
  try {
    ({ stdout } = await execFileAsync(
      "pillbox",
      ["snapshot", "show", resultSnapshot, "--json", "--pillbox", pillboxName],
      { encoding: "utf8", maxBuffer: 64 * 1024 },
    ));
  } catch (error) {
    fail(
      `rustic result snapshot inspection failed: ${error.stderr || error.message}`,
    );
  }
  let inspected;
  try {
    inspected = JSON.parse(stdout);
  } catch {
    fail("rustic result snapshot inspection returned non-JSON output");
  }
  const snapshot = inspected?.snapshot;
  if (
    inspected?.version !== 1 ||
    snapshot?.handle !== resultSnapshot ||
    !Array.isArray(snapshot?.parents) ||
    !snapshot.parents.includes(requestedBaseSnapshot)
  ) {
    fail(
      "managed result snapshot is not descended from the requested base snapshot",
    );
  }
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
    fail(
      `${step.id} executable preflight failed: ${error.stderr || error.message}`,
    );
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

function validateManifest(value) {
  if (!isRecord(value) || value.schema_version !== REPORT_SCHEMA_VERSION)
    fail(`manifest schema_version must be ${REPORT_SCHEMA_VERSION}`);
  const deployment = value.deployment;
  if (
    !isRecord(deployment) ||
    deployment.worker_name !== "pillbox-managed-burnin"
  )
    fail("manifest must target pillbox-managed-burnin");
  if (
    deployment.managed_execution_limit !== 1 ||
    deployment.reviewed_new_execution_count !== 1
  )
    fail("fixed workload allowance must be exactly one reviewed execution");
  if (
    typeof deployment.allowance_epoch !== "string" ||
    deployment.allowance_epoch.length === 0
  )
    fail("manifest allowance_epoch must be a non-empty string");
  if (!isRecord(value.workload) || !Array.isArray(value.workload.steps))
    fail("manifest workload.steps must be an array");
  const stepById = new Map();
  for (const step of value.workload.steps) {
    if (!isRecord(step) || typeof step.id !== "string" || stepById.has(step.id))
      fail("manifest workload steps must have unique string ids");
    stepById.set(step.id, step);
  }
  for (const [stepId, operation] of EXPECTED_WORKLOAD) {
    if (stepById.get(stepId)?.operation !== operation)
      fail(`manifest step ${stepId} must use operation ${operation}`);
  }
  for (const stepId of stepById.keys()) {
    if (!EXPECTED_WORKLOAD.has(stepId))
      fail(`manifest contains unexpected workload step ${stepId}`);
  }
  if (stepById.size !== EXPECTED_WORKLOAD.size)
    fail("manifest must contain exactly the checked burn-in steps");
  const first = stepById.get("first-execute");
  if (!isRecord(first) || !isRecord(first.request))
    fail("manifest must contain first-execute.request");
  if (first.invocation_id !== first.request.invocation_id)
    fail("first-execute invocation identity must match its request");
  const identity = managedBurninEpochIdentity(deployment.allowance_epoch);
  if (
    first.invocation_id !== identity.invocation_id ||
    first.request.session_ref?.session_id !== identity.session_id
  ) {
    fail(
      "first-execute invocation and session identities must derive from allowance_epoch",
    );
  }
  if (stepById.get("exact-retry")?.request_ref !== "first-execute")
    fail("exact-retry must reference first-execute");
  if (first.expected?.allowance_reservation !== 1)
    fail("first-execute must reserve the one reviewed allowance");
  if (
    stepById.get("exact-retry")?.expected?.allowance_reservation_delta !== 0 ||
    stepById.get("exact-retry")?.expected?.new_model_turns !== 0
  )
    fail("exact-retry must not reserve or sample again");
  const inputHash = `sha256:${createHash("sha256").update(first.request.rendered_input).digest("hex")}`;
  if (first.request.rendered_input_hash !== inputHash)
    fail("manifest rendered_input_hash is not deterministic");
  const statusSteps = value.workload.steps.filter(
    (step) => step?.operation === "status",
  );
  for (const [index, step] of statusSteps.entries()) {
    if (
      step.evidence_limit !== MAX_STATUS_PAGE_SIZE ||
      step.evidence_after !== index * MAX_STATUS_PAGE_SIZE ||
      step.expected?.bounded !== true
    )
      fail(`${step.id} must request the checked bounded evidence page`);
  }
  const networkSteps = value.workload.steps.filter(
    (step) => step.operation !== "managed_codex_preflight",
  );
  if (value.workload.expected_network_requests !== networkSteps.length + 2)
    fail(
      "manifest network request count must include both live allowance reads",
    );
  const unsupported = stepById.get("unsupported-managed-codex");
  if (
    unsupported.expected?.error_code !== "unsupported_execution" ||
    unsupported.expected?.provision_attempts !== 0 ||
    unsupported.expected?.network_requests !== 0
  )
    fail("managed Codex preflight must reject without side effects");
  const finalize = stepById.get("finalize");
  if (
    finalize.session_id !== first.request.session_ref.session_id ||
    finalize.requires_operator_request !== true ||
    finalize.expected?.requests !== 1 ||
    finalize.expected?.kill_before_transfer !== true ||
    finalize.expected?.result_snapshot_required !== true
  )
    fail(
      "finalize must be one operator-scoped cleanup for the execution session",
    );
}

function managedBurninEpochIdentity(epoch) {
  const sessionHash = createHash("sha256")
    .update(`pillbox-managed-burnin/session/1\0${epoch}`)
    .digest("hex")
    .slice(0, 32);
  const invocationHash = createHash("sha256")
    .update(`pillbox-managed-burnin/invocation/1\0${epoch}`)
    .digest("hex")
    .slice(0, 32);
  return {
    session_id: `burnin-session-${sessionHash}`,
    invocation_id: `burnin-invocation-${invocationHash}`,
  };
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
  if (index < 0) return undefined;
  const value = args[index + 1];
  if (value === undefined || value.startsWith("--"))
    fail(`${name} requires a value`);
  return value;
}

function ensureTrailingSlash(value) {
  return value.endsWith("/") ? value : `${value}/`;
}

function digestText(value) {
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  return `{${Object.keys(value)
    .sort((left, right) => (left < right ? -1 : left > right ? 1 : 0))
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

function fail(message) {
  console.error(`✗ ${message}`);
  process.exit(1);
}
