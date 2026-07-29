import {
  Validator,
  type Schema,
  type ValidationResult,
} from "@cfworker/json-schema";
import type { JsonValue } from "./huddles_runtime.js";

const MAX_RAW_OUTPUT_BYTES = 1_000_000;

export type RawStructuredOutputInspection =
  | { readonly status: "accepted"; readonly output: string }
  | {
      readonly status: "rejected";
      readonly reason:
        | "missing"
        | "too_large"
        | "invalid_json"
        | "invalid_schema"
        | "schema_violation";
      readonly detail: string;
    };

/**
 * Accept the harness-independent fallback only when the assistant emitted one
 * schema-valid JSON value: bare, in one exact JSON fence, or as the sole
 * schema-valid object/array amid provider prose. The rejection detail never
 * includes model output.
 */
export function inspectRawStructuredOutput(
  text: string | undefined,
  schema: Readonly<Record<string, JsonValue>>,
): RawStructuredOutputInspection {
  if (text === undefined) {
    return { status: "rejected", reason: "missing", detail: "no assistant text" };
  }
  const candidate = text.trim();
  if (candidate.length === 0) {
    return { status: "rejected", reason: "missing", detail: "empty assistant text" };
  }
  if (
    candidate.length > MAX_RAW_OUTPUT_BYTES ||
    new TextEncoder().encode(candidate).byteLength > MAX_RAW_OUTPUT_BYTES
  ) {
    return {
      status: "rejected",
      reason: "too_large",
      detail: `assistant text exceeds ${MAX_RAW_OUTPUT_BYTES} bytes`,
    };
  }
  const jsonText =
    /^```(?:json)?\s*([\s\S]*?)\s*```$/i.exec(candidate)?.[1] ?? candidate;
  let validator: Validator;
  try {
    validator = new Validator(schema as Schema, "2020-12");
  } catch {
    return {
      status: "rejected",
      reason: "invalid_schema",
      detail: "caller schema could not be compiled",
    };
  }
  const validate = (value: unknown): ValidationResult | undefined => {
    try {
      return validator.validate(value);
    } catch {
      return;
    }
  };
  let exactValue: unknown;
  try {
    exactValue = JSON.parse(jsonText);
  } catch {
    const validContainers = jsonContainers(candidate)
      .map(parseJson)
      .filter((value): value is JsonValue => value !== undefined)
      .filter((value) => validate(value)?.valid === true);
    if (validContainers.length === 1) {
      return {
        status: "accepted",
        output: JSON.stringify(validContainers[0]),
      };
    }
    return {
      status: "rejected",
      reason: "invalid_json",
      detail:
        validContainers.length > 1
          ? "assistant text contains multiple schema-valid JSON values"
          : "assistant text contains no schema-valid JSON value",
    };
  }
  const validation = validate(exactValue);
  if (!validation) {
    return {
      status: "rejected",
      reason: "invalid_schema",
      detail: "caller schema could not be evaluated",
    };
  }
  if (!validation.valid) {
    const error = validation.errors[0];
    return {
      status: "rejected",
      reason: "schema_violation",
      detail: error
        ? `${error.instanceLocation || "/"} ${error.keyword}`
        : "JSON value does not satisfy the caller schema",
    };
  }
  return { status: "accepted", output: JSON.stringify(exactValue) };
}

function parseJson(text: string): JsonValue | undefined {
  try {
    return JSON.parse(text) as JsonValue;
  } catch {
    return;
  }
}

/** Balanced top-level JSON containers, respecting strings and escapes. */
function jsonContainers(text: string): string[] {
  const containers: string[] = [];
  let start = -1;
  let stack: string[] = [];
  let inString = false;
  let escaped = false;
  for (let index = 0; index < text.length; index++) {
    const char = text[index];
    if (start < 0) {
      if (char === "{" || char === "[") {
        start = index;
        stack = [char];
      }
      continue;
    }
    if (inString) {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === '"') inString = false;
      continue;
    }
    if (char === '"') {
      inString = true;
      continue;
    }
    if (char === "{" || char === "[") {
      stack.push(char);
      continue;
    }
    if (char !== "}" && char !== "]") continue;
    const open = stack.at(-1);
    if (
      (char === "}" && open !== "{") ||
      (char === "]" && open !== "[")
    ) {
      start = -1;
      stack = [];
      continue;
    }
    stack.pop();
    if (stack.length === 0) {
      containers.push(text.slice(start, index + 1));
      start = -1;
    }
  }
  return containers;
}
