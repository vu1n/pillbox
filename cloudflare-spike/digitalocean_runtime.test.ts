import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import type { ExecuteInvocationV2Request } from "./src/codex_execution.ts";
import type { DigitalOceanApi, DigitalOceanSession } from "./src/digitalocean_client.ts";
import type { RelationalDatabase } from "./src/execution_store.ts";
registerHooks({
  resolve(s, c, next) {
    return next(
      c.parentURL?.includes("/cloudflare-spike/src/") && s.startsWith(".") && s.endsWith(".js")
        ? s.slice(0, -3) + ".ts"
        : s,
      c,
    );
  },
});
const { DigitalOceanExecutionRuntime, decodeDigitalOceanResult } =
  await import("./src/digitalocean_runtime.ts");
const { D1DigitalOceanAllocations } = await import("./src/digitalocean_allocations.ts");
const { DigitalOceanClient } = await import("./src/digitalocean_client.ts");
const { computeRenderedInputHash, computeInvocationRequestHash } =
  await import("./src/codex_execution.ts");
const { DIGITALOCEAN_TRANSPORT, DIGITALOCEAN_ADAPTER_REVISION, DIGITALOCEAN_HARNESS_VERSION } =
  await import("./src/digitalocean_contract.ts");
const configId = "11111111-1111-4111-8111-111111111111";
const providerId = "22222222-2222-4222-8222-222222222222";
const settings = { enabled: "1", token: "secret-token", configId, namespace: "test-realm" };
async function request(): Promise<ExecuteInvocationV2Request> {
  return {
    contract_version: "pillbox.execution/2",
    session_ref: { session_id: "session-a" },
    invocation_id: "invoke-a",
    idempotency_key: "invoke-a",
    rendered_input: "hello",
    rendered_input_hash: await computeRenderedInputHash("hello"),
    tool_policy: "deny_all",
    execution: {
      transport: {
        harness: "opencode",
        transport: DIGITALOCEAN_TRANSPORT,
        harness_version: DIGITALOCEAN_HARNESS_VERSION,
        adapter_revision: DIGITALOCEAN_ADAPTER_REVISION,
      },
      requested: {
        provider: "openrouter",
        model: "openai/gpt-5-mini",
        profile: null,
        reasoning_effort: "high",
      },
      placement: "managed_container",
      context_renderer_revision: "test/1",
    },
    execution_policy_revision: "test/1",
    output_format: { type: "text", retry_count: 0 },
  };
}
const result = {
  harness_version: DIGITALOCEAN_HARNESS_VERSION,
  text: "Hello",
  info: {
    id: "msg-1",
    role: "assistant",
    providerID: "openrouter",
    modelID: "served-model",
    finish: "stop",
    tokens: { input: 10, output: 2, cache: { read: 3, write: 0 } },
    cost: 0.0002,
  },
};
class Api implements DigitalOceanApi {
  creates = 0;
  launches = 0;
  polls = 0;
  deletes = 0;
  finds = 0;
  session: DigitalOceanSession | null = null;
  async create(name: string, config_id: string) {
    this.creates++;
    return (this.session = {
      name,
      config_id,
      session_id: providerId,
      status: "SESSION_STATUS_READY",
    });
  }
  async get() {
    assert.ok(this.session);
    return this.session;
  }
  async find() {
    this.finds++;
    return this.session;
  }
  async exec(_id: string, argv: readonly string[]) {
    if (argv.length > 4) {
      this.launches++;
      return "started\n";
    }
    this.polls++;
    return JSON.stringify(result);
  }
  async remove() {
    this.deletes++;
    this.session = null;
  }
}
function setup(api = new Api()) {
  const db = new DatabaseSync(":memory:");
  db.exec(
    readFileSync(
      new URL("./migrations/0005_digitalocean_allocations.sql", import.meta.url),
      "utf8",
    ),
  );
  const relational = {
    prepare(sql: string) {
      return {
        bind(...values: unknown[]) {
          return {
            async all() {
              const results = db.prepare(sql).all(...(values as never[]));
              return {
                results,
                meta: {
                  rows_read: results.length,
                  rows_written: /^SELECT/.test(sql) ? 0 : results.length,
                },
              };
            },
          };
        },
      };
    },
  } as RelationalDatabase;
  const allocations = new D1DigitalOceanAllocations(relational, () => {});
  const runtime = new DigitalOceanExecutionRuntime({
    api,
    allocations,
    settings,
    delay: async () => {},
  });
  return { db, allocations, runtime, api };
}

test("DO runs once, records actual served model and usage, and deletes its sandbox", async () => {
  const { runtime, api, allocations } = setup();
  const value = await request();
  const output = await runtime.execute(value);
  assert.deepEqual(output.output, { text: "Hello" });
  assert.equal(output.served_model, "openrouter/served-model");
  assert.equal(api.creates, 1);
  assert.equal(api.launches, 1);
  assert.equal(api.deletes, 1);
  assert.equal((await allocations.get(value.invocation_id))?.state, "deleted");
  await assert.rejects(runtime.execute(value), /already exists/);
  await runtime.cleanup(value.invocation_id);
  assert.equal(api.creates, 1);
  assert.equal(api.deletes, 1);
});

test("disabled, unsupported and oversized DO requests cause zero allocation/API calls", async () => {
  const { runtime, api, allocations } = setup();
  const r = await request();
  for (const changed of [
    { ...r, tool_policy: "runtime_default" as const },
    { ...r, rendered_input: "x".repeat(32769) },
    {
      ...r,
      execution: {
        ...r.execution,
        transport: { ...r.execution.transport, harness_version: "future" },
      },
    },
  ])
    assert.ok((await runtime.execute(changed)).error);
  const disabled = new DigitalOceanExecutionRuntime({
    api,
    allocations,
    settings: { ...settings, enabled: "0" },
  });
  assert.equal((await disabled.execute(r)).error?.code, "managed_disabled");
  assert.equal(api.creates, 0);
  assert.equal(await allocations.get(r.invocation_id), null);
});

test("ambiguous create is reconciled by exact name/config and never repeated", async () => {
  const api = new Api();
  const create = api.create.bind(api);
  api.create = async (...args) => {
    await create(...args);
    throw new Error("response lost");
  };
  const { runtime } = setup(api);
  await assert.rejects(runtime.execute(await request()), /response lost/);
  assert.equal(api.creates, 1);
  assert.equal(api.launches, 0);
  assert.equal(api.deletes, 1);
  assert.equal(api.finds, 1);
});

test("absent ambiguous allocation remains stopping for later cleanup", async () => {
  const api = new Api();
  api.create = async () => {
    api.creates++;
    throw new Error("timeout");
  };
  const { runtime, allocations } = setup(api);
  await assert.rejects(runtime.execute(await request()), /unresolved/);
  const row = await allocations.get("invoke-a");
  assert.equal(row?.state, "stopping");
  api.session = {
    session_id: providerId,
    name: row!.name,
    config_id: configId,
    status: "SESSION_STATUS_READY",
  };
  await runtime.cleanup("invoke-a");
  assert.equal(api.deletes, 1);
  assert.equal(api.creates, 1);
});

test("cancellation before allocation leaves a tombstone; it cannot resurrect", async () => {
  const { runtime, api } = setup();
  await runtime.cleanup("invoke-a");
  await assert.rejects(runtime.execute(await request()), /already exists/);
  assert.equal(api.creates, 0);
});

test("cancellation during create fences the late response before sampling", async () => {
  const api = new Api();
  const create = api.create.bind(api);
  let release!: () => void;
  const barrier = new Promise<void>((resolve) => {
    release = resolve;
  });
  api.create = async (...args) => {
    await barrier;
    return create(...args);
  };
  const { runtime, allocations } = setup(api);
  const running = runtime.execute(await request());
  while ((await allocations.get("invoke-a")) === null)
    await new Promise((resolve) => setTimeout(resolve, 1));
  await assert.rejects(runtime.cleanup("invoke-a"), /unresolved/);
  release();
  await assert.rejects(running, /cancelled/);
  assert.equal(api.launches, 0);
  assert.equal(api.deletes, 1);
});

test("lost launch response is cleaned up without another launch", async () => {
  const api = new Api();
  api.exec = async () => {
    api.launches++;
    throw new Error("unknown launch");
  };
  const { runtime } = setup(api);
  await assert.rejects(runtime.execute(await request()), /unknown launch/);
  assert.equal(api.launches, 1);
  assert.equal(api.deletes, 1);
});

test("cleanup failure persists until a read-only recovery deletes the same sandbox", async () => {
  const api = new Api();
  const remove = api.remove.bind(api);
  api.remove = async () => {
    throw new Error("delete failed");
  };
  const { runtime, allocations } = setup(api);
  const result = await runtime.execute(await request());
  assert.equal(result.error?.code, "runtime_failed");
  assert.ok(
    result.evidence.some(
      (event) =>
        typeof event === "object" && event !== null && "type" in event && event.type === "usage",
    ),
  );
  assert.equal((await allocations.get("invoke-a"))?.state, "stopping");
  api.remove = remove;
  await runtime.cleanup("invoke-a");
  assert.equal(api.creates, 1);
  assert.equal(api.launches, 1);
  assert.equal(api.deletes, 1);
});

test("schema output and native usage are validated; request model is not served evidence", async () => {
  const r = {
    ...(await request()),
    output_format: {
      type: "json_schema" as const,
      retry_count: 2 as const,
      schema: { type: "object", required: ["ok"], properties: { ok: { type: "boolean" } } },
    },
  };
  assert.equal(decodeDigitalOceanResult(result, r).error?.code, "structured_output_missing");
  assert.deepEqual(decodeDigitalOceanResult({ ...result, text: '{"ok":true}' }, r).output, {
    json: { ok: true },
  });
  assert.equal(
    decodeDigitalOceanResult({ ...result, text: '{"ok":"invalid"}' }, r).error?.code,
    "structured_output_missing",
  );
  const valid = { ...result, info: { ...result.info, structured: { ok: true } } };
  assert.deepEqual(decodeDigitalOceanResult(valid, r).output, { json: { ok: true } });
  assert.throws(() =>
    decodeDigitalOceanResult(
      { ...result, info: { ...result.info, tokens: { input: -1 } } },
      { ...r, output_format: { type: "text", retry_count: 0 } },
    ),
  );
  const cf = {
    ...r,
    execution: { ...r.execution, transport: { ...r.execution.transport, transport: "http" } },
  };
  assert.notEqual(await computeInvocationRequestHash(r), await computeInvocationRequestHash(cf));
});

test("HTTP client pins DO origin, disables redirects, redacts error bodies, never retries", async () => {
  let calls = 0;
  const client = new DigitalOceanClient("test-secret", async (url, init) => {
    calls++;
    assert.equal(String(url), "https://api.digitalocean.com/v2/agents/sessions");
    assert.equal(init?.redirect, "error");
    assert.equal((init?.headers as Record<string, string>).Authorization, "Bearer test-secret");
    return new Response("test-secret echoed upstream", { status: 503 });
  });
  await assert.rejects(
    client.create("name", configId),
    (error) =>
      error instanceof Error &&
      error.message.includes("503") &&
      !error.message.includes("test-secret"),
  );
  assert.equal(calls, 1);
});

test("HTTP client rejects ambiguous name lookup, oversized bodies, and mismatched creates", async () => {
  const s = {
    session_id: providerId,
    config_id: configId,
    name: "name",
    status: "SESSION_STATUS_READY",
  };
  const client = new DigitalOceanClient("key", async () => Response.json({ sessions: [s, s] }));
  await assert.rejects(client.find("name"), /ambiguous/);
  const huge = new DigitalOceanClient("key", async () => new Response("x".repeat(262145)));
  await assert.rejects(huge.find("name"), /byte limit/);
  const mismatch = new DigitalOceanClient("key", async () => Response.json({ session: s }));
  await assert.rejects(mismatch.create("different", configId), /identity mismatch/);
});

test("DO request hash matches the shared Huddles execution/2 fixture", async () => {
  const fixture = JSON.parse(
    readFileSync(new URL("./testdata/digitalocean-execution.json", import.meta.url), "utf8"),
  );
  const { validateExecuteInvocationV2Request, computeExecutionIdentityDigest } =
    await import("./src/codex_execution.ts");
  const request = await validateExecuteInvocationV2Request(fixture.request);
  assert.equal(await computeInvocationRequestHash(request), fixture.request_hash);
  assert.equal(
    await computeExecutionIdentityDigest(request.execution, request.execution_policy_revision),
    fixture.execution_digest,
  );
});

test("router dispatches by sealed identity and cancels by stored attribution", async () => {
  const { ManagedExecutionRuntime } = await import("./src/managed_runtime.ts");
  const { runtime, api } = setup();
  let cfExec = 0;
  let cfCancel = 0;
  const router = new ManagedExecutionRuntime(
    {
      execute: async () => {
        cfExec++;
        return { served_model: null, output: { text: "CF" }, evidence: [] };
      },
      cancel: async () => {
        cfCancel++;
      },
    },
    runtime,
  );
  const r = await request();
  await router.execute(r);
  assert.equal(api.creates, 1);
  assert.equal(cfExec, 0);
  const cancel = {
    contract_version: "pillbox.execution/2",
    invocation_id: r.invocation_id,
    idempotency_key: r.idempotency_key,
    reason: "stop",
  } as const;
  await router.cancel(cancel, r.session_ref.session_id, {
    harness: "opencode",
    transport: DIGITALOCEAN_TRANSPORT,
    requested_model: "openrouter/openai/gpt-5-mini",
    served_model: null,
  });
  assert.equal(cfCancel, 0);
  await router.execute({
    ...r,
    execution: { ...r.execution, transport: { ...r.execution.transport, transport: "http" } },
  });
  assert.equal(cfExec, 1);
  assert.equal(api.creates, 1);
});

test("all Qwen effort requests are rejected before DO allocation", async () => {
  const { runtime, api, allocations } = setup();
  const r = await request();
  for (const reasoning_effort of ["low", "medium", "high"] as const) {
    const unsupported = {
      ...r,
      execution: {
        ...r.execution,
        requested: { ...r.execution.requested, model: "qwen/qwen3-coder", reasoning_effort },
      },
    };
    assert.equal((await runtime.execute(unsupported)).error?.code, "unsupported_execution");
  }
  assert.equal(api.creates, 0);
  assert.equal(api.launches, 0);
  assert.equal(await allocations.get(r.invocation_id), null);
});

test("DO launch binds the sealed effort into explicit native variant options", async () => {
  for (const reasoning_effort of ["low", "medium", "high"] as const) {
    const api = new Api();
    const exec = api.exec.bind(api);
    api.exec = async (id, argv) => {
      if (argv.length > 4) {
        assert.deepEqual(JSON.parse(argv[4]).reasoning, {
          variant: `pillbox-${reasoning_effort}`,
          options: {
            reasoning: { effort: reasoning_effort },
            provider: { require_parameters: true },
          },
        });
      }
      return exec(id, argv);
    };
    const { runtime } = setup(api);
    const r = await request();
    const output = await runtime.execute({
      ...r,
      execution: { ...r.execution, requested: { ...r.execution.requested, reasoning_effort } },
    });
    assert.equal(output.error, undefined);
    assert.equal(api.launches, 1);
  }
});

test("router preserves Cloudflare reasoning admission before allocation", async () => {
  const { ManagedExecutionRuntime } = await import("./src/managed_runtime.ts");
  const { OpencodeExecutionRuntime } = await import("./src/execution_service.ts");
  const cf = new OpencodeExecutionRuntime({
    sandboxFor: () => {
      throw new Error("unsupported model allocated a sandbox");
    },
    configFor: () => {
      throw new Error("unsupported model reached config");
    },
  });
  const { runtime } = setup();
  const router = new ManagedExecutionRuntime(cf, runtime);
  const r = await request();
  const rejected = {
    ...r,
    execution: {
      ...r.execution,
      transport: { ...r.execution.transport, transport: "http" },
      requested: { ...r.execution.requested, model: "qwen/qwen3-coder" },
    },
  };
  assert.equal(router.preflight(rejected)?.code, "unsupported_execution");
  assert.equal((await router.execute(rejected)).error?.code, "unsupported_execution");
});
