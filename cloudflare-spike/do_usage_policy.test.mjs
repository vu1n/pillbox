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
  assert.doesNotMatch(worker, /SessionGateway|routeAgentRequest|from ["']agents["']/);
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

  const rustSource = read("../src/events/source.rs");
  const rustSink = read("../src/events/sink.rs");
  assert.doesNotMatch(rustSource, /ManagedDoSource|managed_endpoint/);
  assert.doesNotMatch(rustSink, /ManagedDoSink|managed_endpoint/);
});
