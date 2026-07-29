import assert from "node:assert/strict";
import { test } from "node:test";
import { inspectRawStructuredOutput } from "./src/structured_output.ts";

const schema = {
  type: "object",
  properties: {
    kind: { type: "string", enum: ["document"] },
    text: { type: "string", minLength: 1 },
  },
  required: ["kind", "text"],
  additionalProperties: false,
} as const;

test("a bare schema-valid JSON value is accepted and normalized", () => {
  assert.deepEqual(
    inspectRawStructuredOutput(
      '  { "kind": "document", "text": "# Grill" }  ',
      schema,
    ),
    {
      status: "accepted",
      output: '{"kind":"document","text":"# Grill"}',
    },
  );
});

test("one exact JSON fence is accepted", () => {
  assert.deepEqual(
    inspectRawStructuredOutput(
      '```json\n{"kind":"document","text":"# Grill"}\n```',
      schema,
    ),
    {
      status: "accepted",
      output: '{"kind":"document","text":"# Grill"}',
    },
  );
});

test("the sole schema-valid container is normalized from provider prose", () => {
  assert.deepEqual(
    inspectRawStructuredOutput(
      'Here it is:\n```json\n{"kind":"document","text":"# Grill"}\n```',
      schema,
    ),
    {
      status: "accepted",
      output: '{"kind":"document","text":"# Grill"}',
    },
  );
  assert.deepEqual(
    inspectRawStructuredOutput(
      'First {"kind":"document","text":"one"} then {"kind":"document","text":"two"}',
      schema,
    ),
    {
      status: "rejected",
      reason: "invalid_json",
      detail: "assistant text contains multiple schema-valid JSON values",
    },
  );
});

test("schema violations and malformed schemas are rejected without model content", () => {
  assert.deepEqual(
    inspectRawStructuredOutput(
      '{"kind":"document","text":"","extra":true}',
      schema,
    ),
    {
      status: "rejected",
      reason: "schema_violation",
      detail: "# properties",
    },
  );
  assert.deepEqual(
    inspectRawStructuredOutput(
      '{"kind":"document","text":"","extra":true}',
      { type: "not-a-json-schema-type" },
    ),
    {
      status: "rejected",
      reason: "schema_violation",
      detail: "# type",
    },
  );
  assert.deepEqual(
    inspectRawStructuredOutput(undefined, schema),
    {
      status: "rejected",
      reason: "missing",
      detail: "no assistant text",
    },
  );
  assert.deepEqual(
    inspectRawStructuredOutput('{"kind":"document","text":"ok"}', {
      type: "not-a-json-schema-type",
    }),
    {
      status: "rejected",
      reason: "schema_violation",
      detail: "# type",
    },
  );
});
