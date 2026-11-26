import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { registerHooks } from "node:module";
import { test } from "node:test";

// Production source uses emitted `.js` specifiers; map local source imports to `.ts`.
registerHooks({
  resolve(specifier, context, nextResolve) {
    return nextResolve(
      context.parentURL?.includes("/cloudflare-spike/src/") &&
        specifier.startsWith(".") &&
        specifier.endsWith(".js")
        ? `${specifier.slice(0, -3)}.ts`
        : specifier,
      context,
    );
  },
});

const { enforceOpencodeToolPolicy, opencodePromptTools } = await import(
  "./src/tool_policy.ts"
);
const { safeRuntimeDiagnostic } = await import("./src/runtime_diagnostic.ts");
const { structuredOutputRetryPrompt } = await import("./src/structured_output.ts");
const { deriveSandboxRuntimeId } = await import("./src/runtime_identity.ts");
const { publicControllerOwner } = await import("./src/managed_ownership.ts");

test("deny_all OpenCode policy denies every server permission and prompt tool", () => {
  assert.deepEqual(
    enforceOpencodeToolPolicy(
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

  const tools = opencodePromptTools("deny_all");
  assert.ok(Object.keys(tools).length > 0);
  assert.ok(Object.values(tools).every((allowed) => allowed === false));
});

test("tool policies other than deny_all fail closed", () => {
  assert.throws(
    () => enforceOpencodeToolPolicy({ permission: "allow" }, "runtime_default"),
    /unsupported tool policy: runtime_default/,
  );
  assert.throws(
    () => opencodePromptTools("runtime_default"),
    /unsupported tool policy: runtime_default/,
  );
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
    safeRuntimeDiagnostic(
      new Error(
        "provider https://api.example.test/v1 failed with cfat_secret-token-value and abcdefghijklmnopqrstuvwxyz0123456789",
      ),
    ),
    "Error: provider [url redacted] failed with [credential redacted] and [opaque value redacted]",
  );
  assert.equal(
    safeRuntimeDiagnostic({ reason: "opaque" }),
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

test("public controller ownership is stable per subject and keeps its persisted digest", async () => {
  const owner = await publicControllerOwner({ subject: "controller:alice" });
  // Pinned: existing D1 rows carry this digest, so the owner hash must never drift.
  assert.deepEqual(owner, {
    domain: "public_controller",
    digest: "sha256:e8faa0e9d128b80d087e6d20eb92c01902f10b60ba2ceeffed77a2eabe529391",
  });
  assert.deepEqual(await publicControllerOwner({ subject: "controller:alice" }), owner);
  assert.notDeepEqual(await publicControllerOwner({ subject: "controller:bob" }), owner);
});

test("managed execution service keeps the admission policy", async () => {
  const source = await readFile(new URL("./src/managed_execution.ts", import.meta.url), "utf8");
  assert.match(
    source,
    /admission: managedAdmissionPolicy\(env\.MANAGED_EXECUTION_ENABLED\)/,
  );
});

test("public provisioning keeps the admission guard", async () => {
  const worker = await readFile(new URL("./src/worker.ts", import.meta.url), "utf8");

  const provisionGuard = worker.indexOf(
    'new URL(req.url).pathname === "/v2/workspaces/provision"',
  );
  assert.ok(provisionGuard >= 0);
  assert.ok(worker.indexOf("requireManagedAdmission", provisionGuard) >= provisionGuard);
  assert.ok(
    worker.indexOf("routeWorkspaceTransfer(req, env)", provisionGuard) > provisionGuard,
  );
});
