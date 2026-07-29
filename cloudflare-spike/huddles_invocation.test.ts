import assert from "node:assert/strict";
import { test } from "node:test";
import {
  classifyRunningInvocation,
  enforceHuddlesOpencodePolicy,
  huddlesPromptTools,
  isHuddlesSessionName,
} from "./src/huddles_policy.ts";

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
