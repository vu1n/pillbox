import { createHash } from "node:crypto";
import { open, readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const GRAPHQL_ENDPOINT = "https://api.cloudflare.com/client/v4/graphql";
export const MAX_ROW_COUNT = 100;
export const MAX_REQUESTS = 4;
export const MAX_RESPONSE_BYTES = 1024 * 1024;
export const REQUEST_TIMEOUT_MS = 30_000;
const MAX_JSON_DEPTH = 32;

const LIMITATIONS = Object.freeze([
  "provider results are delayed and hourly/sample-granular",
  "lifecycle stop was not independently confirmed; this is a snapshot, not release proof",
  "R2 HTTP attempts require the separate opt-in operation capture",
  "no execute/cleanup partition inference, receipt completeness, or billing total is made",
]);
const SOURCE_NAMES = ["do_lifecycle", "container_lifecycle", "retention_before", "retention_after"];
const ID_PATTERNS = {
  account_tag: /^[0-9a-f]{32}$/,
  namespace_id: /^[0-9a-f]{32}$/,
  object_id: /^[0-9a-f]{64}$/,
  instance_id: /^[0-9a-f]{64}$/,
  application_id: /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
};

export class PlanError extends Error {
  constructor(message) {
    super(message);
    this.name = "PlanError";
    this.code = "invalid_plan";
  }
}

const isRecord = value => value !== null && typeof value === "object" && !Array.isArray(value);
const hasOnly = (value, keys, path) => {
  if (!isRecord(value)) throw new PlanError(`${path} must be an object`);
  for (const key of Object.keys(value)) if (!keys.includes(key)) throw new PlanError(`${path}.${key} is not allowed`);
  for (const key of keys) if (!Object.hasOwn(value, key)) throw new PlanError(`${path}.${key} is required`);
};

function canonicalUtc(value, path) {
  if (typeof value !== "string" || !Number.isFinite(Date.parse(value)) || new Date(value).toISOString() !== value) {
    throw new PlanError(`${path} must be canonical ISO UTC`);
  }
  return value;
}

export function validatePlan(value) {
  hasOnly(value, ["schema_version", "account_tag", "namespace_id", "object_id", "application_id", "instance_id", "windows"], "plan");
  if (value.schema_version !== 1) throw new PlanError("plan.schema_version must be 1");
  for (const [name, pattern] of Object.entries(ID_PATTERNS)) {
    if (typeof value[name] !== "string" || !pattern.test(value[name])) throw new PlanError(`plan.${name} has the wrong identifier format`);
  }
  hasOnly(value.windows, ["before", "lifecycle", "after"], "plan.windows");
  const windows = {};
  for (const name of ["before", "lifecycle", "after"]) {
    hasOnly(value.windows[name], ["from", "to"], `plan.windows.${name}`);
    const from = canonicalUtc(value.windows[name].from, `plan.windows.${name}.from`);
    const to = canonicalUtc(value.windows[name].to, `plan.windows.${name}.to`);
    if (Date.parse(from) >= Date.parse(to)) throw new PlanError(`plan.windows.${name} requires from < to`);
    if (Date.parse(to) - Date.parse(from) > 24 * 60 * 60 * 1000) throw new PlanError(`plan.windows.${name} exceeds the 24 hour bound`);
    windows[name] = { from, to };
  }
  if (Date.parse(windows.before.to) > Date.parse(windows.lifecycle.from)) throw new PlanError("before and lifecycle windows overlap");
  if (Date.parse(windows.lifecycle.to) > Date.parse(windows.after.from)) throw new PlanError("lifecycle and after windows overlap");
  return {
    schema_version: 1,
    account_tag: value.account_tag,
    namespace_id: value.namespace_id,
    object_id: value.object_id,
    application_id: value.application_id,
    instance_id: value.instance_id,
    windows,
  };
}

function canonicalJson(value) {
  const visit = (entry, depth) => {
    // The source wrapper adds one level to the already bounded response.
    if (depth > MAX_JSON_DEPTH + 1) throw new Error("response depth exceeds bound");
    if (Array.isArray(entry)) return `[${entry.map(item => visit(item, depth + 1)).join(",")}]`;
    if (isRecord(entry)) return `{${Object.keys(entry).sort().map(key => `${JSON.stringify(key)}:${visit(entry[key], depth + 1)}`).join(",")}}`;
    return JSON.stringify(entry);
  };
  return visit(value, 0);
}

function exceedsJsonDepth(value) {
  const pending = [{ value, depth: 0 }];
  while (pending.length > 0) {
    const { value: entry, depth } = pending.pop();
    if (depth > MAX_JSON_DEPTH) return true;
    if (Array.isArray(entry)) {
      for (const child of entry) pending.push({ value: child, depth: depth + 1 });
    } else if (isRecord(entry)) {
      for (const child of Object.values(entry)) pending.push({ value: child, depth: depth + 1 });
    }
  }
  return false;
}

function digest(value) {
  return `sha256:${createHash("sha256").update(canonicalJson(value)).digest("hex")}`;
}

function quoted(value) {
  return JSON.stringify(value);
}

function sourceResource(plan, name) {
  if (name === "do_lifecycle") return `account:${plan.account_tag}/namespace:${plan.namespace_id}/object:${plan.object_id}`;
  if (name === "container_lifecycle") return `account:${plan.account_tag}/application:${plan.application_id}/instance:${plan.instance_id}`;
  return `account:${plan.account_tag}/namespace:${plan.namespace_id}`;
}

function sourceBase(plan, name, query, window) {
  return {
    status: "unavailable",
    reason: "not_observed",
    dataset: "cloudflare_graphql",
    resource_id: sourceResource(plan, name),
    window: { ...window },
    query,
    http_status: null,
    response: null,
    capture_sha256: digest({ query, http_status: null, response: null }),
  };
}

function sourceRecord(base, status, reason, httpStatus, response) {
  const { reason: _baseReason, ...withoutReason } = base;
  const source = { ...withoutReason, status, ...(reason ? { reason } : {}), http_status: httpStatus, response };
  source.capture_sha256 = digest({ query: source.query, http_status: httpStatus, response });
  return source;
}

function metadataResponse(reason, extra = {}) {
  return { bounded: false, reason, ...extra };
}

async function readBoundedResponse(response) {
  const contentLength = Number(response?.headers?.get?.("content-length"));
  if (Number.isFinite(contentLength) && contentLength > MAX_RESPONSE_BYTES) {
    return { ok: false, reason: "response_too_large", body: metadataResponse("response_too_large", { content_length: contentLength }) };
  }
  if (typeof response?.body?.getReader === "function") {
    const reader = response.body.getReader();
    const chunks = [];
    let bytes = 0;
    try {
      while (true) {
        const part = await reader.read();
        if (part.done) break;
        if (!(part.value instanceof Uint8Array)) {
          try { await reader.cancel(); } catch { /* bounded cleanup only */ }
          return { ok: false, reason: "response_unreadable", body: metadataResponse("response_unreadable") };
        }
        bytes += part.value.byteLength;
        if (bytes > MAX_RESPONSE_BYTES) {
          try { await reader.cancel(); } catch { /* bounded cleanup only */ }
          return { ok: false, reason: "response_too_large", body: metadataResponse("response_too_large", { bytes }) };
        }
        chunks.push(part.value);
      }
    } catch {
      try { await reader.cancel(); } catch { /* bounded cleanup only */ }
      return { ok: false, reason: "response_unreadable", body: metadataResponse("response_unreadable") };
    }
    const text = Buffer.concat(chunks).toString("utf8");
    let body;
    try { body = JSON.parse(text); } catch { return { ok: false, reason: "malformed_json", body: metadataResponse("malformed_json", { bytes }) }; }
    if (exceedsJsonDepth(body)) return { ok: false, reason: "response_depth", body: metadataResponse("response_depth", { bytes }) };
    return { ok: true, body };
  }
  return { ok: false, reason: "response_unreadable", body: metadataResponse("response_unreadable") };
}

async function raceWithTimeout(operation, milliseconds) {
  let timer;
  try {
    return await Promise.race([
      Promise.resolve().then(operation).then(value => ({ timedOut: false, value })),
      new Promise(resolve => { timer = setTimeout(() => resolve({ timedOut: true }), milliseconds); }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function requestProvider(plan, name, query, window, options) {
  const base = sourceBase(plan, name, query, window);
  if (!options.token) return { source: sourceRecord(base, "unavailable", "missing_token", null, null) };
  const controller = new AbortController();
  const deadline = Date.now() + options.timeoutMs;
  const timer = setTimeout(() => controller.abort(), options.timeoutMs);
  try {
    const fetchOutcome = await raceWithTimeout(() => options.fetchImpl(GRAPHQL_ENDPOINT, {
        method: "POST",
        redirect: "error",
        headers: { authorization: `Bearer ${options.token}`, "content-type": "application/json" },
        body: JSON.stringify({ query }),
        signal: controller.signal,
      }), options.timeoutMs);
    if (fetchOutcome.timedOut || controller.signal.aborted) return { source: sourceRecord(base, "unavailable", "timeout", null, metadataResponse("timeout")) };
    const response = fetchOutcome.value;
    const httpStatus = Number.isInteger(response?.status) ? response.status : null;
    const remaining = deadline - Date.now();
    if (remaining <= 0) return { source: sourceRecord(base, "unavailable", "timeout", null, metadataResponse("timeout")) };
    const bodyOutcome = await raceWithTimeout(() => readBoundedResponse(response), remaining);
    const parsed = bodyOutcome.timedOut ? { ok: false, reason: "timeout", body: metadataResponse("timeout") } : bodyOutcome.value;
    if (!parsed.ok) return { source: sourceRecord(base, "unavailable", parsed.reason, httpStatus, parsed.body) };
    if (httpStatus === null || httpStatus < 200 || httpStatus >= 300) {
      return { source: sourceRecord(base, "unavailable", "http_error", httpStatus, parsed.body), body: parsed.body };
    }
    if (parsed.body?.errors !== undefined && parsed.body.errors !== null &&
      (!Array.isArray(parsed.body.errors) || parsed.body.errors.length > 0)) {
      return { source: sourceRecord(base, "unavailable", "graphql_error", httpStatus, parsed.body), body: parsed.body };
    }
    return { source: sourceRecord(base, "available", null, httpStatus, parsed.body), body: parsed.body };
  } catch (error) {
    const reason = controller.signal.aborted ? "timeout" : "transport_error";
    return { source: sourceRecord(base, "unavailable", reason, null, metadataResponse(reason)) };
  } finally {
    clearTimeout(timer);
    controller.abort();
  }
}

function queryFor(plan, name) {
  const account = `accountTag:${quoted(plan.account_tag)}`;
  if (name === "do_lifecycle") {
    const { from, to } = plan.windows.lifecycle;
    return `query { viewer { accounts(filter:{${account}}) { accountTag durableObjectsPeriodicGroups(limit:${MAX_ROW_COUNT},filter:{namespaceId:${quoted(plan.namespace_id)},objectId:${quoted(plan.object_id)},datetime_geq:${quoted(from)},datetime_lt:${quoted(to)}},orderBy:[datetimeHour_ASC]) { dimensions { namespaceId objectId datetimeHour } sum { duration rowsRead rowsWritten } } } } }`;
  }
  if (name === "container_lifecycle") {
    const { from, to } = plan.windows.lifecycle;
    return `query { viewer { accounts(filter:{${account}}) { accountTag containersUsageAdaptiveGroups(limit:${MAX_ROW_COUNT},filter:{applicationId:${quoted(plan.application_id)},instanceId:${quoted(plan.instance_id)},datetime_geq:${quoted(from)},datetime_lt:${quoted(to)}},orderBy:[datetimeHour_ASC]) { dimensions { applicationId instanceId datetimeHour location region } sum { cpuTimeSec allocatedMemory allocatedDisk txBytes } } } } }`;
  }
  const window = plan.windows[name === "retention_before" ? "before" : "after"];
  return `query { viewer { accounts(filter:{${account}}) { accountTag durableObjectsSqlStorageGroups(limit:${MAX_ROW_COUNT},filter:{namespaceId:${quoted(plan.namespace_id)},datetime_geq:${quoted(window.from)},datetime_lt:${quoted(window.to)}},orderBy:[datetimeHour_ASC]) { dimensions { namespaceId datetimeHour } max { storedBytes } } } } }`;
}

function unavailable(reason, rowCount) {
  return { status: "unavailable", reason, ...(Number.isInteger(rowCount) ? { row_count: rowCount } : {}) };
}

function numeric(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function counter(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function accountRows(result, key, plan) {
  if (!result.body || !isRecord(result.body)) return { error: result.source.reason ?? "missing_response" };
  const accounts = result.body?.data?.viewer?.accounts;
  if (!Array.isArray(accounts) || accounts.length !== 1) return { error: accounts?.length === 0 ? "wrong_identifier" : "missing_response" };
  if (!isRecord(accounts[0]) || accounts[0].accountTag !== plan.account_tag) return { error: "wrong_identifier" };
  const rows = accounts[0][key];
  if (!Array.isArray(rows)) return { error: "missing_rows" };
  if (rows.length === 0) return { error: "empty_rows", rowCount: 0 };
  if (rows.length >= MAX_ROW_COUNT) return { error: "row_limit", rowCount: rows.length };
  return { rows };
}

function measureDo(result, plan) {
  if (result.source.status !== "available") return unavailable(result.source.reason);
  const found = accountRows(result, "durableObjectsPeriodicGroups", plan);
  if (found.error) return unavailable(found.error, found.rowCount);
  const units = { sql_rows_read: 0, sql_rows_written: 0, duration_gb_seconds: 0 };
  for (const row of found.rows) {
    const dimensions = row?.dimensions;
    if (!isRecord(dimensions) || dimensions.namespaceId !== plan.namespace_id || dimensions.objectId !== plan.object_id) return unavailable("wrong_identifier", found.rows.length);
    const sum = row?.sum;
    if (!isRecord(sum) || !counter(sum.rowsRead) || !counter(sum.rowsWritten) || !numeric(sum.duration)) return unavailable("missing_or_invalid_numeric", found.rows.length);
    units.sql_rows_read += sum.rowsRead;
    units.sql_rows_written += sum.rowsWritten;
    units.duration_gb_seconds += sum.duration;
  }
  if (!Number.isSafeInteger(units.sql_rows_read) || !Number.isSafeInteger(units.sql_rows_written) || !numeric(units.duration_gb_seconds)) return unavailable("numeric_overflow", found.rows.length);
  return { status: "available", row_count: found.rows.length, units };
}

function measureContainer(result, plan) {
  if (result.source.status !== "available") return unavailable(result.source.reason);
  const found = accountRows(result, "containersUsageAdaptiveGroups", plan);
  if (found.error) return unavailable(found.error, found.rowCount);
  const units = { cpu_time_sec: 0, allocated_memory: 0, allocated_disk: 0, tx_bytes: 0 };
  for (const row of found.rows) {
    const dimensions = row?.dimensions;
    if (!isRecord(dimensions) || dimensions.applicationId !== plan.application_id || dimensions.instanceId !== plan.instance_id) return unavailable("wrong_identifier", found.rows.length);
    const sum = row?.sum;
    if (!isRecord(sum) || !numeric(sum.cpuTimeSec) || !numeric(sum.allocatedMemory) || !numeric(sum.allocatedDisk) || !counter(sum.txBytes)) return unavailable("missing_or_invalid_numeric", found.rows.length);
    units.cpu_time_sec += sum.cpuTimeSec;
    units.allocated_memory += sum.allocatedMemory;
    units.allocated_disk += sum.allocatedDisk;
    units.tx_bytes += sum.txBytes;
  }
  if (!numeric(units.cpu_time_sec) || !numeric(units.allocated_memory) || !numeric(units.allocated_disk) || !Number.isSafeInteger(units.tx_bytes)) return unavailable("numeric_overflow", found.rows.length);
  return { status: "available", row_count: found.rows.length, units };
}

function measureRetention(result, plan) {
  if (result.source.status !== "available") return unavailable(result.source.reason);
  const found = accountRows(result, "durableObjectsSqlStorageGroups", plan);
  if (found.error) return unavailable(found.error, found.rowCount);
  let storedBytes = -Infinity;
  for (const row of found.rows) {
    if (!isRecord(row?.dimensions) || row.dimensions.namespaceId !== plan.namespace_id) return unavailable("wrong_identifier", found.rows.length);
    const value = row?.max?.storedBytes;
    if (!counter(value)) return unavailable("missing_or_invalid_numeric", found.rows.length);
    storedBytes = Math.max(storedBytes, value);
  }
  return { status: "available", row_count: found.rows.length, units: { stored_bytes: storedBytes } };
}

function capturedAt(value) {
  const date = typeof value === "function" ? value() : value ?? new Date();
  const parsed = date instanceof Date ? date : new Date(date);
  if (!Number.isFinite(parsed.getTime())) throw new Error("invalid capture clock");
  return parsed.toISOString();
}

export async function captureVendorLifecycle(planInput, options = {}) {
  const plan = validatePlan(planInput);
  const token = Object.hasOwn(options, "token") ? (options.token ?? "") : (process.env.CLOUDFLARE_API_TOKEN ?? "");
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  if (token && typeof fetchImpl !== "function") throw new Error("fetch implementation unavailable");
  const timeoutMs = options.timeoutMs ?? REQUEST_TIMEOUT_MS;
  if (!Number.isFinite(timeoutMs) || timeoutMs < 1 || timeoutMs > 60_000) throw new Error("timeout is outside the bounded range");
  const results = {};
  for (const name of SOURCE_NAMES) {
    const query = queryFor(plan, name);
    const window = plan.windows[name === "do_lifecycle" || name === "container_lifecycle" ? "lifecycle" : name === "retention_before" ? "before" : "after"];
    results[name] = await requestProvider(plan, name, query, window, { token, fetchImpl, timeoutMs });
  }
  const observations = {
    do_lifecycle: measureDo(results.do_lifecycle, plan),
    container_lifecycle: measureContainer(results.container_lifecycle, plan),
    retention_before: measureRetention(results.retention_before, plan),
    retention_after: measureRetention(results.retention_after, plan),
  };
  const sources = Object.fromEntries(SOURCE_NAMES.map(name => {
    const source = results[name].source;
    const observation = observations[name];
    return [name, observation.status === "available" ? source : {
      ...source,
      status: "unavailable",
      reason: observation.reason,
    }];
  }));
  return {
    schema_version: 1,
    capture_type: "cloudflare_vendor_lifecycle",
    status: Object.values(observations).every(value => value.status === "available") ? "available" : "unavailable",
    captured_at: capturedAt(options.now),
    plan,
    bounds: { max_window_hours: 24, row_cap: MAX_ROW_COUNT, max_requests: MAX_REQUESTS, max_response_bytes: MAX_RESPONSE_BYTES },
    observations,
    sources,
    limitations: [...LIMITATIONS],
  };
}

function parseCli(argv) {
  const values = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help") return { help: true };
    if (arg !== "--plan" && arg !== "--out") throw new PlanError("unknown CLI argument");
    const value = argv[++index];
    if (!value) throw new PlanError(`${arg} requires a value`);
    if (values[arg.slice(2)] !== undefined) throw new PlanError(`${arg} was repeated`);
    values[arg.slice(2)] = value;
  }
  if (!values.plan || !values.out) throw new PlanError("--plan and --out are required");
  return values;
}

async function loadPlan(argument) {
  const text = argument.trim().startsWith("{") ? argument : await readFile(resolve(process.cwd(), argument), "utf8");
  try { return JSON.parse(text); } catch { throw new PlanError("plan is not valid JSON"); }
}

function cliFailure(reason) {
  return {
    schema_version: 1,
    capture_type: "cloudflare_vendor_lifecycle",
    status: "unavailable",
    captured_at: new Date().toISOString(),
    failure: { reason },
    bounds: { max_window_hours: 24, row_cap: MAX_ROW_COUNT, max_requests: MAX_REQUESTS, max_response_bytes: MAX_RESPONSE_BYTES },
    limitations: [...LIMITATIONS],
  };
}

export async function runCli(argv = process.argv.slice(2), env = process.env) {
  let args;
  try { args = parseCli(argv); } catch {
    process.stderr.write("invalid arguments; use --help\n");
    return 2;
  }
  if (args.help) {
    process.stdout.write("Usage: node capture-vendor-lifecycle.mjs --plan <path-or-json> --out <exclusive-output>\n");
    return 0;
  }
  const outputPath = resolve(process.cwd(), args.out);
  let handle;
  try { handle = await open(outputPath, "wx", 0o600); } catch {
    process.stderr.write("cannot create exclusive output\n");
    return 2;
  }
  let result;
  try {
    const plan = await loadPlan(args.plan);
    result = await captureVendorLifecycle(plan, { token: env.CLOUDFLARE_API_TOKEN });
  } catch (error) {
    result = cliFailure(error instanceof PlanError ? "invalid_plan" : "capture_failed");
  }
  try {
    await handle.writeFile(`${JSON.stringify(result)}\n`, "utf8");
    await handle.chmod(0o600);
  } finally {
    await handle.close();
  }
  return result.status === "available" ? 0 : 1;
}

const invoked = process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url;
if (invoked) runCli().then(code => { process.exitCode = code; });
