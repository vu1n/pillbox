/**
 * Validation for the bounded `workspace --capture-r2-json` protocol.
 *
 * This module is deliberately pure.  It never talks to R2, stores capture
 * records, or includes helper diagnostics in a returned value.
 */

export const MAX_R2_CAPTURE_STDOUT_BYTES = 512 * 1024;
export const MAX_R2_CAPTURE_RECORDS = 256;
export const MAX_R2_CAPTURE_STRING_BYTES = 1_024;

const CAPTURE_REASON_CODES = [
  "record_limit",
  "unknown_action",
  "foreign_selector",
  "transport_error",
  "redirect",
  "response_error",
  "response_incomplete",
  "operation_failed",
  "counter_overflow",
  "capture_unavailable",
] as const;

const METHODS = ["GET", "HEAD", "PUT", "POST", "DELETE", "OTHER"] as const;
const ACTIONS = [
  "GetObject",
  "HeadObject",
  "HeadBucket",
  "ListObjectsV2",
  "PutObject",
  "DeleteObject",
  "CreateMultipartUpload",
  "UploadPart",
  "CompleteMultipartUpload",
  "AbortMultipartUpload",
  "unknown",
] as const;

const CAPTURE_OPERATIONS = [
  "snapshot_restore",
  "snapshot_finalize",
  "verification",
] as const;

export type R2CaptureReason = (typeof CAPTURE_REASON_CODES)[number];
export type R2CaptureMethod = (typeof METHODS)[number];
export type R2CaptureAction = (typeof ACTIONS)[number];
export type R2CaptureOperation = (typeof CAPTURE_OPERATIONS)[number];

export interface R2CaptureRecord {
  readonly id: string;
  readonly method: R2CaptureMethod;
  readonly action: R2CaptureAction;
  readonly selector: string | null;
  readonly status: number | null;
  readonly request_body_bytes: number;
  readonly response_body_bytes: number;
  readonly response_complete: boolean;
}

export interface R2HttpOperationCapture {
  readonly schema_version: 1;
  readonly capture_type: "r2_http_operation";
  readonly capture_id: string;
  readonly operation: R2CaptureOperation;
  readonly bucket: string;
  readonly prefix: string;
  readonly started_at: string;
  readonly finished_at: string;
  readonly status: "complete" | "incomplete";
  readonly reasons: readonly R2CaptureReason[];
  readonly records: readonly R2CaptureRecord[];
}

export interface R2CaptureExpectation {
  readonly operation: R2CaptureOperation;
  readonly bucket: string;
  readonly prefix: string;
}

export const R2_CAPTURE_UNAVAILABLE_REASONS = [
  "replayed",
  "helper_response_missing",
  "helper_response_malformed",
  "helper_response_oversized",
  "capture_invalid",
] as const;

export type R2CaptureUnavailableReason =
  (typeof R2_CAPTURE_UNAVAILABLE_REASONS)[number];

export interface R2CaptureUnavailable {
  readonly status: "unavailable";
  readonly reason: R2CaptureUnavailableReason;
}

export type R2CaptureTelemetry = R2HttpOperationCapture | R2CaptureUnavailable;

export type ParsedR2CaptureOutput =
  | {
      readonly status: "completed" | "failed";
      readonly snapshot: string | null;
      readonly capture: R2CaptureTelemetry;
    }
  | R2CaptureUnavailable;

/**
 * Parse and validate a capture document against the transfer's R2 scope.
 * Invalid or foreign data returns null; no untrusted selector is returned.
 */
export function parseR2Capture(
  value: unknown,
  expected: R2CaptureExpectation,
): R2HttpOperationCapture | null {
  const expectedPrefix = normalizeR2Prefix(expected.prefix);
  if (
    !isBoundedString(expected.bucket) ||
    expected.bucket.length === 0 ||
    expectedPrefix.length === 0
  ) {
    return null;
  }
  if (!isRecord(value) || !hasExactKeys(value, [
    "schema_version",
    "capture_type",
    "capture_id",
    "operation",
    "bucket",
    "prefix",
    "started_at",
    "finished_at",
    "status",
    "reasons",
    "records",
  ])) {
    return null;
  }

  if (
    value.schema_version !== 1 ||
    value.capture_type !== "r2_http_operation" ||
    typeof value.capture_id !== "string" ||
    !isUuid(value.capture_id) ||
    value.operation !== expected.operation ||
    !isBoundedString(value.bucket) ||
    !isBoundedString(value.prefix) ||
    value.bucket !== expected.bucket ||
    value.prefix !== expectedPrefix ||
    !isUtcTimestamp(value.started_at) ||
    !isUtcTimestamp(value.finished_at) ||
    !timestampOrderIsValid(value.started_at, value.finished_at) ||
    (value.status !== "complete" && value.status !== "incomplete") ||
    !Array.isArray(value.reasons) ||
    !Array.isArray(value.records) ||
    value.records.length > MAX_R2_CAPTURE_RECORDS
  ) {
    return null;
  }

  const reasons: R2CaptureReason[] = [];
  for (const reason of value.reasons) {
    if (!isCaptureReason(reason) || reasons.includes(reason)) return null;
    reasons.push(reason);
  }
  if (reasons.length > 10) return null;
  if (value.status === "complete" && reasons.length !== 0) return null;
  if (value.status === "incomplete" && reasons.length === 0) return null;

  const records: R2CaptureRecord[] = [];
  for (let index = 0; index < value.records.length; index += 1) {
    const record = parseRecord(value.records[index], value.capture_id, index + 1, expected);
    if (record === null) return null;
    records.push(record);
  }

  const hasUnknownAction = records.some((record) => record.action === "unknown");
  if (
    hasUnknownAction &&
    !reasons.includes("unknown_action") &&
    !reasons.includes("foreign_selector")
  ) {
    return null;
  }
  if (reasons.includes("record_limit") && records.length !== MAX_R2_CAPTURE_RECORDS) {
    return null;
  }
  if (
    value.status === "complete" &&
    records.some(
      (record) =>
        record.action === "unknown" ||
        record.status === null ||
        !record.response_complete ||
        (record.status >= 300 && record.status < 400),
    )
  ) {
    return null;
  }

  return {
    schema_version: 1,
    capture_type: "r2_http_operation",
    capture_id: value.capture_id,
    operation: expected.operation,
    bucket: value.bucket,
    prefix: value.prefix,
    started_at: value.started_at,
    finished_at: value.finished_at,
    status: value.status,
    reasons,
    records,
  };
}

/**
 * Parse the helper's complete stdout envelope. The transfer status and
 * snapshot are authoritative independently of diagnostic capture validity;
 * malformed or missing capture data is represented as unavailable telemetry.
 * Malformed envelope/result fields still return a bounded stable reason and
 * never expose stdout or stderr.
 */
export function parseR2CaptureOutput(
  stdout: string | null | undefined,
  expected: R2CaptureExpectation,
): ParsedR2CaptureOutput {
  if (typeof stdout !== "string" || stdout.length === 0) {
    return unavailable("helper_response_missing");
  }
  if (exceedsUtf8Limit(stdout, MAX_R2_CAPTURE_STDOUT_BYTES)) {
    return unavailable("helper_response_oversized");
  }

  let value: unknown;
  try {
    value = JSON.parse(stdout);
  } catch {
    return unavailable("helper_response_malformed");
  }
  if (!isRecord(value) || !hasExactKeys(value, ["schema_version", "status", "snapshot"], ["capture"])) {
    return unavailable("helper_response_malformed");
  }
  if (
    value.schema_version !== 1 ||
    (value.status !== "completed" && value.status !== "failed") ||
    (value.snapshot !== null &&
      (typeof value.snapshot !== "string" || !/^[0-9a-f]{64}$/.test(value.snapshot)))
  ) {
    return unavailable("helper_response_malformed");
  }
  if (value.status === "failed" && value.snapshot !== null) {
    return unavailable("helper_response_malformed");
  }
  if (
    value.status === "completed" &&
    (expected.operation === "snapshot_finalize") !== (value.snapshot !== null)
  ) {
    return unavailable("helper_response_malformed");
  }

  const capture = parseR2Capture(value.capture, expected);
  return {
    status: value.status,
    snapshot: value.snapshot,
    capture:
      capture === null || (value.status === "failed" && capture.status === "complete")
        ? unavailableR2Capture("capture_invalid")
        : capture,
  };
}

export function unavailableR2Capture(
  reason: R2CaptureUnavailableReason,
): R2CaptureUnavailable {
  return { status: "unavailable", reason };
}

/** Normalize the scope used by capture-v1 to a slash-terminated prefix. */
export function normalizeR2Prefix(prefix: string): string {
  const trimmed = prefix.replace(/^\/+|\/+$/g, "");
  return trimmed.length === 0 ? "" : `${trimmed}/`;
}

function parseRecord(
  value: unknown,
  captureId: string,
  sequence: number,
  expected: R2CaptureExpectation,
): R2CaptureRecord | null {
  if (!isRecord(value) || !hasExactKeys(value, [
    "id",
    "method",
    "action",
    "selector",
    "status",
    "request_body_bytes",
    "response_body_bytes",
    "response_complete",
  ])) {
    return null;
  }
  const method = value.method;
  const action = value.action;
  const selector = value.selector;
  const status = value.status;
  const requestBodyBytes = value.request_body_bytes;
  const responseBodyBytes = value.response_body_bytes;
  const responseComplete = value.response_complete;
  const normalizedStatus =
    status === null ? null : isHttpStatus(status) ? status : undefined;
  if (
    typeof value.id !== "string" ||
    value.id !== `${captureId}:${sequence}` ||
    !isMethod(method) ||
    !isAction(action) ||
    (selector !== null && !isBoundedString(selector)) ||
    normalizedStatus === undefined ||
    !isSafeNonnegativeInteger(requestBodyBytes) ||
    !isSafeNonnegativeInteger(responseBodyBytes) ||
    typeof responseComplete !== "boolean"
  ) {
    return null;
  }

  if (action === "unknown") {
    if (selector !== null) return null;
  } else if (selector === null || !isSafeSelector(selector, action, expected)) {
    return null;
  }
  if (action !== "unknown" && !actionUsesMethod(action, method)) return null;
  if (responseComplete && normalizedStatus === null) return null;

  return {
    id: value.id,
    method,
    action,
    selector,
    status: normalizedStatus,
    request_body_bytes: requestBodyBytes,
    response_body_bytes: responseBodyBytes,
    response_complete: responseComplete,
  };
}

function isSafeSelector(
  selector: string,
  action: R2CaptureAction,
  expected: R2CaptureExpectation,
): boolean {
  if (selector.includes("\u0000") || /[\u0001-\u001f\u007f]/u.test(selector)) return false;
  if (action === "HeadBucket") return selector === "";
  const prefix = normalizeR2Prefix(expected.prefix);
  return prefix === "" || selector === prefix || selector.startsWith(prefix);
}

function actionUsesMethod(
  action: Exclude<R2CaptureAction, "unknown">,
  method: R2CaptureMethod,
): boolean {
  switch (action) {
    case "GetObject":
    case "ListObjectsV2":
      return method === "GET";
    case "HeadObject":
    case "HeadBucket":
      return method === "HEAD";
    case "PutObject":
    case "UploadPart":
      return method === "PUT";
    case "DeleteObject":
    case "AbortMultipartUpload":
      return method === "DELETE";
    case "CreateMultipartUpload":
    case "CompleteMultipartUpload":
      return method === "POST";
  }
}

function isCaptureReason(value: unknown): value is R2CaptureReason {
  return typeof value === "string" &&
    (CAPTURE_REASON_CODES as readonly string[]).includes(value);
}

function isMethod(value: unknown): value is R2CaptureMethod {
  return typeof value === "string" && (METHODS as readonly string[]).includes(value);
}

function isAction(value: unknown): value is R2CaptureAction {
  return typeof value === "string" && (ACTIONS as readonly string[]).includes(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasExactKeys(
  value: Record<string, unknown>,
  requiredKeys: readonly string[],
  optionalKeys: readonly string[] = [],
): boolean {
  const actual = Object.keys(value).sort();
  const allowed = new Set([...requiredKeys, ...optionalKeys]);
  if (requiredKeys.some((key) => !Object.prototype.hasOwnProperty.call(value, key))) return false;
  return actual.every((key) => allowed.has(key));
}

function isBoundedString(value: unknown): value is string {
  return typeof value === "string" && !exceedsUtf8Limit(value, MAX_R2_CAPTURE_STRING_BYTES);
}

function isSafeNonnegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function isHttpStatus(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 100 && (value as number) <= 599;
}

function exceedsUtf8Limit(value: string, maxBytes: number): boolean {
  // UTF-8 uses at least one byte per JS code unit. Avoid allocating an
  // encoded copy for obviously oversized untrusted strings.
  return value.length > maxBytes || new TextEncoder().encode(value).byteLength > maxBytes;
}

function isUuid(value: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value);
}

function isUtcTimestamp(value: unknown): value is string {
  return typeof value === "string" && parseUtcTimestamp(value) !== null;
}

function timestampOrderIsValid(startedAt: string, finishedAt: string): boolean {
  const started = parseUtcTimestamp(startedAt);
  const finished = parseUtcTimestamp(finishedAt);
  return started !== null && finished !== null && started <= finished;
}

function parseUtcTimestamp(value: string): bigint | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?Z$/.exec(value);
  if (match === null) return null;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const hour = Number(match[4]);
  const minute = Number(match[5]);
  const second = Number(match[6]);
  if (
    month < 1 || month > 12 ||
    hour > 23 || minute > 59 || second > 59
  ) return null;

  const date = new Date(0);
  date.setUTCFullYear(year, month - 1, day);
  date.setUTCHours(hour, minute, second, 0);
  if (
    date.getUTCFullYear() !== year ||
    date.getUTCMonth() !== month - 1 ||
    date.getUTCDate() !== day ||
    date.getUTCHours() !== hour ||
    date.getUTCMinutes() !== minute ||
    date.getUTCSeconds() !== second
  ) return null;

  const fraction = (match[7] ?? "").padEnd(9, "0");
  return BigInt(date.getTime()) * 1_000_000n + BigInt(fraction);
}

function unavailable(reason: R2CaptureUnavailableReason): R2CaptureUnavailable {
  return unavailableR2Capture(reason);
}
