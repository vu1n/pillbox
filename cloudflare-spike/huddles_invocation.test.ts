import assert from "node:assert/strict";
import { test } from "node:test";
import {
  classifyRunningInvocation,
  enforceHuddlesOpencodePolicy,
  huddlesPromptTools,
  isHuddlesSessionName,
  safeHuddlesRuntimeDiagnostic,
  structuredOutputRetryPrompt,
} from "./src/huddles_policy.ts";
import { deriveSandboxRuntimeId } from "./src/runtime_identity.ts";

test("reserved Huddles session names are exact", () => {
  assert.equal(isHuddlesSessionName(`ensure-${"a".repeat(64)}`), true);
  assert.equal(isHuddlesSessionName(`ensure-${"A".repeat(64)}`), false);
  assert.equal(isHuddlesSessionName(`ensure-${"a".repeat(63)}`), false);
  assert.equal(isHuddlesSessionName("ordinary-session"), false);
});

test("Huddles OpenCode policy denies every server permission and prompt tool", () => {
  assert.deepEqual(
    enforceHuddlesOpencodePolicy(
      {
        permission: "allow",
        provider: { openai: { options: { apiKey: "secret" } } },
      },
      "deny_all",
    ),
    {
      permission: "deny",
      provider: { openai: { options: { apiKey: "secret" } } },
    },
  );

  const tools = huddlesPromptTools("deny_all");
  assert.ok(Object.keys(tools).length > 0);
  assert.ok(Object.values(tools).every((allowed) => allowed === false));
});

test("only the current isolate owner may report a durable invocation as running", () => {
  assert.equal(classifyRunningInvocation(true), "running");
  assert.equal(classifyRunningInvocation(false), "interrupted");
});

test("structured-output retries preserve the sealed prompt and demand the schema tool", () => {
  assert.equal(
    structuredOutputRetryPrompt("Produce GRILL.md.", 2, 2),
    `Produce GRILL.md.

Structured-output retry 2 of 2: the previous attempt ended without calling StructuredOutput. Call StructuredOutput exactly once with a JSON value that matches the supplied schema. If this runtime cannot expose StructuredOutput, return exactly one bare JSON value matching that schema, with no prose or Markdown fence.`,
  );
  assert.throws(
    () => structuredOutputRetryPrompt("prompt", 0, 2),
    /invalid structured-output retry ordinal/,
  );
});

test("managed invocation diagnostics retain the cause without exposing credentials", () => {
  assert.equal(
    safeHuddlesRuntimeDiagnostic(
      new Error(
        "provider https://api.example.test/v1 failed with cfat_secret-token-value and abcdefghijklmnopqrstuvwxyz0123456789",
      ),
    ),
    "Error: provider [url redacted] failed with [credential redacted] and [opaque value redacted]",
  );
  assert.equal(
    safeHuddlesRuntimeDiagnostic({ reason: "opaque" }),
    "unknown runtime error",
  );
});

test("sandbox runtime identity preserves SessionRef identity without exceeding Cloudflare limits", async () => {
  assert.equal(
    await deriveSandboxRuntimeId("ordinary-session"),
    "ordinary-session",
  );

  const sessionId = `ensure-${"a".repeat(64)}`;
  const runtimeId = await deriveSandboxRuntimeId(sessionId);
  assert.equal(runtimeId.length, 63);
  assert.match(runtimeId, /^pbx-[0-9a-f]{59}$/);
  assert.equal(await deriveSandboxRuntimeId(sessionId), runtimeId);
  assert.notEqual(runtimeId, sessionId);

  assert.match(await deriveSandboxRuntimeId("api"), /^pbx-[0-9a-f]{59}$/);
  assert.match(await deriveSandboxRuntimeId("-unsafe"), /^pbx-[0-9a-f]{59}$/);
});
