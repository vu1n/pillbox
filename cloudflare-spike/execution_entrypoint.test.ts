import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

test("managed entrypoint is v2-only", async () => {
  const source = await readFile(new URL("./src/huddles_runtime.ts", import.meta.url), "utf8");
  const managedStart = source.indexOf("export class HuddlesRuntimeEntrypoint");
  const managed = source.slice(managedStart, source.indexOf("export function executionService"));

  assert.ok(managedStart >= 0);
  assert.match(managed, /async executeInvocation\(/);
  assert.match(managed, /async getExecutionStatus\(/);
  assert.match(managed, /async cancelInvocation\(/);
  assert.doesNotMatch(source, /ensureSession|invokeSession/);
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

test("private Huddles execution validates burn-in bootstrap before service construction", async () => {
  const runtime = await readFile(
    new URL("./src/huddles_runtime.ts", import.meta.url),
    "utf8",
  );
  const factory = runtime.indexOf("export function executionService(");
  const bootstrap = runtime.indexOf("requireManagedBurninBootstrap(env)", factory);
  const store = runtime.indexOf("new D1ExecutionStore", factory);

  assert.ok(factory >= 0);
  assert.ok(bootstrap > factory);
  assert.ok(store > bootstrap);
});
