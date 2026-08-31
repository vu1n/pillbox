import assert from "node:assert/strict";
import { test } from "node:test";
import {
  readBoundedJson,
  readBoundedJsonWithDigest,
  RequestBodyTooLargeError,
} from "./src/request_body.ts";

test("bounded JSON accepts a body at the byte limit", async () => {
  const body = JSON.stringify({ value: "ok" });
  assert.deepEqual(
    await readBoundedJson(
      new Request("https://example.test", { method: "POST", body }),
      new TextEncoder().encode(body).byteLength,
    ),
    { value: "ok" },
  );
});

test("bounded JSON digest binds the exact request bytes", async () => {
  const compact = await readBoundedJsonWithDigest(
    new Request("https://example.test", { method: "POST", body: '{"a":1}' }),
  );
  const spaced = await readBoundedJsonWithDigest(
    new Request("https://example.test", { method: "POST", body: '{ "a": 1 }' }),
  );
  assert.deepEqual(compact.value, spaced.value);
  assert.notEqual(compact.sha256, spaced.sha256);
});

test("bounded JSON rejects declared and streamed overflow", async () => {
  await assert.rejects(
    readBoundedJson(
      new Request("https://example.test", {
        method: "POST",
        headers: { "content-length": "100" },
        body: "{}",
      }),
      10,
    ),
    RequestBodyTooLargeError,
  );
  await assert.rejects(
    readBoundedJson(
      new Request("https://example.test", { method: "POST", body: '{"long":"value"}' }),
      5,
    ),
    RequestBodyTooLargeError,
  );
});
