import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import {
  computeExecutionIdentityDigest,
  computeRenderedInputHash,
  type ExecuteInvocationV2Request,
  type InvocationExecution,
  UnsupportedAcpExecutionError,
  validateExecuteInvocationV2Request,
} from "./src/codex_execution.ts";
import type {
  AcpClient,
  AcpEventEnvelope,
  AcpPromptParams,
  AcpPromptResult,
  AcpSessionNewParams,
} from "./src/acp_turn.ts";

// Production source uses emitted `.js` specifiers. Node's type-stripping test
// runner executes the `.ts` sources directly, so map only local source imports.
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

const { AcpProcessCrashedError, AcpTurnDriver } = await import(
  "./src/acp_turn.ts"
);

const acpExecution: InvocationExecution = {
  transport: {
    harness: "codex",
    transport: "acp",
    harness_version: "0.1.0",
    adapter_revision: "pillbox/acp-spike-1",
  },
  requested: {
    provider: "openai-codex",
    model: "gpt-5.6-sol",
    profile: "sol",
    reasoning_effort: "high",
  },
  placement: "managed_container",
  context_renderer_revision: "planning-partner/cc-011",
};

const output_format = {
  type: "json_schema" as const,
  schema: { type: "object", properties: { ok: { type: "boolean" } } },
  retry_count: 2 as const,
};

async function validRequest(
  changes: Partial<ExecuteInvocationV2Request> = {},
): Promise<ExecuteInvocationV2Request> {
  const rendered_input = changes.rendered_input ?? "Write the sealed answer.";
  return validateExecuteInvocationV2Request({
    contract_version: "pillbox.execution/2",
    session_ref: { session_id: "session-acp" },
    invocation_id: "invocation-acp-1",
    idempotency_key: "delivery-acp-1",
    rendered_input,
    rendered_input_hash: await computeRenderedInputHash(rendered_input),
    tool_policy: "deny_all",
    execution: changes.execution ?? acpExecution,
    execution_policy_revision: "planning-execution/acp/1",
    output_format,
    ...changes,
  });
}

class Deferred<T> {
  readonly promise: Promise<T>;
  resolve!: (value: T) => void;

  constructor() {
    this.promise = new Promise<T>((resolve) => {
      this.resolve = resolve;
    });
  }
}

class FakeAcpClient implements AcpClient {
  readonly calls: string[] = [];
  readonly session_params: AcpSessionNewParams[] = [];
  readonly prompt_params: AcpPromptParams[] = [];
  readonly cancel_params: { readonly session_id: string }[] = [];
  prompt_gate?: Deferred<AcpPromptResult>;
  crash_next_prompt = false;

  async initialize(): Promise<void> {
    this.calls.push("initialize");
  }

  async session_new(params: AcpSessionNewParams) {
    this.calls.push("session_new");
    this.session_params.push(params);
    return { session_id: "acp-session-1" };
  }

  async prompt(params: AcpPromptParams): Promise<AcpPromptResult> {
    this.calls.push("prompt");
    this.prompt_params.push(params);
    params.on_event({ type: "text_delta", text: "evidence" });
    if (this.crash_next_prompt) {
      this.crash_next_prompt = false;
      throw new AcpProcessCrashedError("fake ACP child exited");
    }
    if (this.prompt_gate !== undefined) return this.prompt_gate.promise;
    return { output: { ok: true } };
  }

  async cancel(params: { readonly session_id: string }): Promise<void> {
    this.calls.push("cancel");
    this.cancel_params.push(params);
    this.prompt_gate?.resolve({ output: { cancelled: true } });
  }

  async cleanup(): Promise<void> {
    this.calls.push("cleanup");
  }

  async respawn(): Promise<void> {
    this.calls.push("respawn");
  }
}

test("ACP sends only empty MCP and the exact sealed prompt, with attribution", async () => {
  const request = await validRequest({ rendered_input: "café 🐕\nexact bytes" });
  const client = new FakeAcpClient();
  const events: AcpEventEnvelope[] = [];
  const result = await new AcpTurnDriver(client).execute(request, {
    appendAcpEvent: (event) => events.push(event),
  });

  assert.equal(result.status, "completed");
  assert.deepEqual(client.session_params, [{ mcpServers: [] }]);
  assert.equal(client.prompt_params.length, 1);
  const prompt = client.prompt_params[0];
  assert.equal(prompt.text, request.rendered_input);
  assert.deepEqual(Object.keys(prompt).sort(), ["on_event", "session_id", "text"]);
  assert.equal("execution" in prompt, false);
  assert.equal("context" in prompt, false);
  assert.equal(events.length, 1);
  assert.equal(events[0].attribution.session_ref.session_id, "session-acp");
  assert.equal(events[0].attribution.invocation_id, "invocation-acp-1");
  assert.equal(
    events[0].attribution.execution_digest,
    await computeExecutionIdentityDigest(
      request.execution,
      request.execution_policy_revision,
    ),
  );
  assert.equal(
    events[0].attribution.execution_policy_revision,
    request.execution_policy_revision,
  );
});

test("a second active turn returns runtime_busy and is never queued", async () => {
  const client = new FakeAcpClient();
  const started = new Deferred<void>();
  client.prompt_gate = new Deferred<AcpPromptResult>();
  const originalPrompt = client.prompt.bind(client);
  client.prompt = async (params) => {
    started.resolve();
    return originalPrompt(params);
  };
  const driver = new AcpTurnDriver(client);
  const first = driver.execute(await validRequest(), { appendAcpEvent: () => {} });
  await started.promise;

  const second = await driver.execute(
    await validRequest({ invocation_id: "invocation-acp-2" }),
    { appendAcpEvent: () => {} },
  );
  assert.deepEqual(second, {
    status: "failed",
    error: { code: "runtime_busy", message: "an ACP invocation is already active" },
  });
  assert.equal(client.prompt_params.length, 1);
  client.prompt_gate.resolve({ output: { ok: true } });
  assert.equal((await first).status, "completed");
});

test("cancellation calls ACP cancel before bounded cleanup", async () => {
  const client = new FakeAcpClient();
  client.prompt_gate = new Deferred<AcpPromptResult>();
  const driver = new AcpTurnDriver(client, 50);
  const running = driver.execute(await validRequest(), { appendAcpEvent: () => {} });
  while (client.prompt_params.length === 0) await new Promise((resolve) => setTimeout(resolve, 0));

  const cancelled = await driver.cancelActiveTurn();
  assert.equal(cancelled?.status, "cancelled");
  assert.deepEqual(client.calls.slice(-2), ["cancel", "cleanup"]);
  assert.equal(client.cancel_params[0].session_id, "acp-session-1");
  assert.equal((await running).status, "cancelled");
});

test("a hung ACP cancel still reaches bounded cleanup", async () => {
  const client = new FakeAcpClient();
  client.prompt_gate = new Deferred<AcpPromptResult>();
  client.cancel = async (params) => {
    client.calls.push("cancel");
    client.cancel_params.push(params);
    setTimeout(() => client.prompt_gate?.resolve({ output: { cancelled: true } }), 10);
    await new Promise<void>(() => {});
  };
  const driver = new AcpTurnDriver(client, 5);
  const running = driver.execute(await validRequest(), { appendAcpEvent: () => {} });
  while (client.prompt_params.length === 0) await new Promise((resolve) => setTimeout(resolve, 0));

  const cancelled = await driver.cancelActiveTurn();
  assert.equal(cancelled?.status, "cancelled");
  assert.deepEqual(client.calls.slice(-2), ["cancel", "cleanup"]);
  assert.equal((await running).status, "cancelled");
});

test("ACP completion is checked against the sealed output schema", async () => {
  const client = new FakeAcpClient();
  client.prompt = async () => ({ output: { ok: "not-a-boolean" } });
  const result = await new AcpTurnDriver(client).execute(
    await validRequest(),
    { appendAcpEvent: () => {} },
  );
  assert.equal(result.status, "failed");
  if (result.status === "failed") {
    assert.equal(result.error.code, "runtime_failed");
    assert.match(result.error.message, /properties|type/);
    assert.doesNotMatch(result.error.message, /not-a-boolean/);
  }

  client.prompt = async () => ({});
  const missing = await new AcpTurnDriver(client).execute(
    await validRequest({ invocation_id: "invocation-acp-missing-output" }),
    { appendAcpEvent: () => {} },
  );
  assert.equal(missing.status, "failed");
  if (missing.status === "failed") {
    assert.equal(missing.error.code, "structured_output_missing");
  }
});

test("a child crash interrupts the current turn and respawns only for the next one", async () => {
  const client = new FakeAcpClient();
  client.crash_next_prompt = true;
  const driver = new AcpTurnDriver(client);
  const first = await driver.execute(await validRequest(), { appendAcpEvent: () => {} });
  assert.equal(first.status, "failed");
  if (first.status === "failed") assert.equal(first.error.code, "runtime_interrupted");
  assert.equal(client.calls.filter((call) => call === "prompt").length, 1);

  const second = await driver.execute(
    await validRequest({ invocation_id: "invocation-acp-2" }),
    { appendAcpEvent: () => {} },
  );
  assert.equal(second.status, "completed");
  assert.deepEqual(client.calls.slice(-5), [
    "cleanup",
    "respawn",
    "initialize",
    "session_new",
    "prompt",
  ]);
});

test("the ACP driver rejects native app-server execution", async () => {
  const request = await validRequest({
    execution: {
      ...acpExecution,
      transport: { ...acpExecution.transport, transport: "app_server" },
    },
  });
  assert.throws(
    () => new AcpTurnDriver(new FakeAcpClient()).execute(request, { appendAcpEvent: () => {} }),
    (error: unknown) =>
      error instanceof UnsupportedAcpExecutionError && error.code === "unsupported_execution",
  );
});
