import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { test } from "node:test";

const read = (path) => readFileSync(new URL(path, import.meta.url), "utf8");

test("managed topology binds only Cloudflare Sandbox as a Durable Object", () => {
  for (const config of [
    read("./wrangler.toml"),
    read("./wrangler.container.toml"),
    read("./wrangler.runtime-test.toml"),
  ]) {
    const bindings = [...config.matchAll(/\[\[durable_objects\.bindings\]\][\s\S]*?class_name\s*=\s*"([^"]+)"/g)]
      .map((match) => match[1]);
    assert.deepEqual(bindings, config.includes('name = "Sandbox"') ? ["Sandbox"] : []);
  }
  assert.equal(existsSync(new URL("./src/session_gateway.ts", import.meta.url)), false);
  const worker = read("./src/worker.ts");
  assert.doesNotMatch(
    worker,
    /SessionGateway|routeAgentRequest|proxyToSandbox|from ["']agents["']/,
  );
});

test("managed dependencies contain no custom agent or Computer runtime", () => {
  const pkg = JSON.parse(read("./package.json"));
  const dependencies = { ...pkg.dependencies, ...pkg.devDependencies };
  assert.equal(dependencies.agents, undefined);
  assert.equal(dependencies["@cloudflare/computer"], undefined);
});

test("execution persistence is bounded and local logs cannot route to a DO", () => {
  const migration = read("./migrations/0001_execution.sql");
  assert.match(migration, /CREATE TABLE execution/);
  assert.doesNotMatch(
    migration,
    /event_(json|payload)|delta_(text|json)|pty_(frame|bytes)|progress_(json|text)/i,
  );

  const service = read("./src/execution_service.ts");
  assert.match(service, /MAX_EVIDENCE_PAGE_SIZE/);
  assert.match(
    service,
    /planned_analytics_points:\s*this\.analytics === undefined \? 0 : 1/,
  );

  assert.equal(existsSync(new URL("../src/events/source.rs", import.meta.url)), false);
  assert.equal(existsSync(new URL("../src/events/sink.rs", import.meta.url)), false);
  assert.doesNotMatch(read("../src/events/mod.rs"), /mod (source|sink);/);
  assert.match(
    read("../src/events/transcripts/tailer.rs"),
    /log: Option<SessionLog>/,
  );
});

test("workspace finalize quiesces prompt-controlled processes before credentials enter", () => {
  const source = read("./src/workspace_transfer.ts");
  const kill = source.indexOf("await sandbox.killAllProcesses()");
  const transfer = source.indexOf("await execWorkspaceTool(");
  assert.ok(kill >= 0 && transfer > kill);
  assert.match(source, /r2\\\.cloudflarestorage\\\.com/);
  assert.match(source, /verifyManagedCapability/);
  assert.match(source, /workspace\.snapshot/);
  assert.match(source, /--parent/);
  assert.doesNotMatch(source, /verifyActorToken|ACTOR_TOKEN_SECRET/);
});

test("public execution uses scoped capabilities and denies runtime tools", () => {
  const source = read("./src/worker.ts");
  assert.match(source, /verifyManagedCapability/);
  assert.match(source, /tool_policy !== "deny_all"/);
  assert.match(source, /readBoundedJson/);
  assert.match(source, /request_sha256/);
  assert.doesNotMatch(source, /verifyActorToken|ACTOR_TOKEN_SECRET/);
  assert.match(read("./src/execution_service.ts"), /request\.tool_policy !== "deny_all"/);
});
