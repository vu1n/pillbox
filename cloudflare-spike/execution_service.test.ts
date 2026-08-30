import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import {
  computeExecutionIdentityDigest,
  computeInvocationRequestHash,
  computeRenderedInputHash,
  type CancelInvocationV2Request,
  type ExecuteInvocationV2Request,
  type ExecutionArtifactRef,
  type JsonValue,
} from "./src/codex_execution.ts";
import type {
  ExecutionArtifact,
  ExecutionArtifactStore,
} from "./src/execution_artifacts.ts";
import type {
  ExecutionRuntime,
  RuntimeTurnResult,
} from "./src/execution_service.ts";
import type {
  ExecutionClaim,
  ExecutionClaimInput,
  ExecutionRecord,
  ExecutionStore,
  FinishExecutionInput,
} from "./src/execution_store.ts";

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

const { EXECUTION_OWNER_LEASE_MS, ExecutionService } = await import(
  "./src/execution_service.ts"
);

async function request(
  changes: Partial<ExecuteInvocationV2Request> = {},
): Promise<ExecuteInvocationV2Request> {
  const rendered_input = changes.rendered_input ?? "Produce a JSON result.";
  return {
    contract_version: "pillbox.execution/2",
    session_ref: { session_id: "session-1" },
    invocation_id: "invocation-1",
    idempotency_key: "delivery-1",
    rendered_input,
    rendered_input_hash: await computeRenderedInputHash(rendered_input),
    tool_policy: "deny_all",
    execution: {
      transport: {
        harness: "opencode",
        transport: "http",
        harness_version: "1.0.0",
        adapter_revision: "pillbox/1",
      },
      requested: {
        provider: "zai-coding-plan",
        model: "glm-4.5-air",
        profile: null,
        reasoning_effort: "high",
      },
      placement: "managed_container",
      context_renderer_revision: "test/1",
    },
    execution_policy_revision: "managed/1",
    output_format: {
      type: "json_schema",
      schema: { type: "object" },
      retry_count: 2,
    },
    ...changes,
  };
}

test("created execution persists terminal evidence and exact retry does not resample", async () => {
  const store = new MemoryStore();
  const artifacts = new MemoryArtifacts();
  const runtime = new FakeRuntime({
    served_model: "zai-coding-plan/glm-4.5-air",
    output: { json: { ok: true } },
    evidence: [
      { type: "message_start", messageId: "m1" },
      { type: "message_delta", messageId: "m1", text: "done" },
    ],
  });
  const service = new ExecutionService(store, artifacts, runtime, fixedOptions());
  const input = await request();

  const created = await service.executeInvocation(input);
  assert.equal(created.status, "completed");
  assert.equal(created.disposition, "created");
  assert.equal(created.attribution.harness, "opencode");
  assert.equal(created.evidence.events.length, 2);
  assert.ok(created.evidence.artifact_ref);

  const reused = await service.executeInvocation(input);
  assert.equal(reused.status, "completed");
  assert.equal(reused.disposition, "reused");
  assert.equal(runtime.executions, 1);
  assert.equal(artifacts.writes, 1);
});

test("changed content conflicts without crossing into the runtime", async () => {
  const runtime = new FakeRuntime({
    served_model: null,
    output: { text: "done" },
    evidence: [],
  });
  const service = new ExecutionService(
    new MemoryStore(),
    new MemoryArtifacts(),
    runtime,
    fixedOptions(),
  );
  await service.executeInvocation(await request());
  const conflict = await service.executeInvocation(
    await request({ rendered_input: "Different sealed input." }),
  );
  assert.equal(conflict.status, "conflict");
  if (conflict.status === "conflict") {
    assert.equal(conflict.error.code, "idempotency_conflict");
  }
  assert.equal(runtime.executions, 1);
});

test("concurrent exact retry observes running and never samples twice", async () => {
  const pending = deferred<RuntimeTurnResult>();
  const runtime = new FakeRuntime(pending.promise);
  const service = new ExecutionService(
    new MemoryStore(),
    new MemoryArtifacts(),
    runtime,
    fixedOptions(),
  );
  const input = await request();
  const first = service.executeInvocation(input);
  await runtime.started;

  const retry = await service.executeInvocation(input);
  assert.equal(retry.status, "running");
  assert.equal(retry.disposition, "reused");
  assert.equal(runtime.executions, 1);

  pending.resolve({ served_model: null, output: { text: "done" }, evidence: [] });
  assert.equal((await first).status, "completed");
});

test("expired running claims become interrupted instead of resampling", async () => {
  const input = await request();
  const store = new MemoryStore();
  await seedRunning(store, input, 0);
  const runtime = new FakeRuntime({
    served_model: null,
    output: { text: "must not run" },
    evidence: [],
  });
  const service = new ExecutionService(store, new MemoryArtifacts(), runtime, {
    now: () => EXECUTION_OWNER_LEASE_MS + 1,
    ownerToken: () => "unused-owner",
  });

  const result = await service.executeInvocation(input);
  assert.equal(result.status, "interrupted");
  assert.equal(runtime.executions, 0);
});

test("status evidence reads are paginated and bounded", async () => {
  const service = new ExecutionService(
    new MemoryStore(),
    new MemoryArtifacts(),
    new FakeRuntime({
      served_model: null,
      output: { text: "done" },
      evidence: [{ n: 0 }, { n: 1 }, { n: 2 }],
    }),
    fixedOptions(),
  );
  await service.executeInvocation(await request());
  const page = await service.getExecutionStatus({
    contract_version: "pillbox.execution/2",
    invocation_id: "invocation-1",
    evidence_after: 1,
    evidence_limit: 1,
  });
  assert.deepEqual(page.evidence.events, [{ n: 1 }]);
  assert.equal(page.evidence.next, 2);
  assert.equal(page.evidence.truncated, true);
});

test("cancellation terminalizes once and exact retries read the same result", async () => {
  const input = await request();
  const store = new MemoryStore();
  await seedRunning(store, input, 1_000);
  const runtime = new FakeRuntime({
    served_model: null,
    output: { text: "must not run" },
    evidence: [],
  });
  const service = new ExecutionService(
    store,
    new MemoryArtifacts(),
    runtime,
    fixedOptions(),
  );
  const cancel = {
    contract_version: "pillbox.execution/2",
    invocation_id: input.invocation_id,
    idempotency_key: "cancel-1",
    reason: "caller stopped the run",
  } as const;

  assert.equal((await service.cancelInvocation(cancel)).status, "cancelled");
  assert.equal((await service.cancelInvocation(cancel)).status, "cancelled");
  assert.equal(runtime.cancellations, 1);
});

async function seedRunning(
  store: MemoryStore,
  input: ExecuteInvocationV2Request,
  now_ms: number,
): Promise<void> {
  await store.claim({
    invocation_id: input.invocation_id,
    idempotency_key: input.idempotency_key,
    request_hash: await computeInvocationRequestHash(input),
    execution_digest: await computeExecutionIdentityDigest(
      input.execution,
      input.execution_policy_revision,
    ),
    execution_policy_revision: input.execution_policy_revision,
    session_id: input.session_ref.session_id,
    attribution: {
      harness: input.execution.transport.harness,
      transport: input.execution.transport.transport,
      requested_model: `${input.execution.requested.provider}/${input.execution.requested.model}`,
      served_model: null,
    },
    owner_token: "seed-owner",
    now_ms,
    lease_expires_at_ms: now_ms + EXECUTION_OWNER_LEASE_MS,
  });
}

function fixedOptions() {
  return { now: () => 1_000, ownerToken: () => "owner-1" };
}

class FakeRuntime implements ExecutionRuntime {
  executions = 0;
  cancellations = 0;
  private readonly result: RuntimeTurnResult | Promise<RuntimeTurnResult>;
  private start!: () => void;
  readonly started = new Promise<void>((resolve) => {
    this.start = resolve;
  });

  constructor(result: RuntimeTurnResult | Promise<RuntimeTurnResult>) {
    this.result = result;
  }

  async execute(): Promise<RuntimeTurnResult> {
    this.executions += 1;
    this.start();
    return this.result;
  }

  async cancel(_request: CancelInvocationV2Request): Promise<void> {
    this.cancellations += 1;
  }
}

class MemoryStore implements ExecutionStore {
  readonly rows = new Map<string, ExecutionRecord>();

  async claim(input: ExecutionClaimInput): Promise<ExecutionClaim> {
    const existing =
      this.rows.get(input.invocation_id) ??
      [...this.rows.values()].find(
        (row) => row.idempotency_key === input.idempotency_key,
      );
    if (existing !== undefined) {
      const exact =
        existing.invocation_id === input.invocation_id &&
        existing.idempotency_key === input.idempotency_key &&
        existing.request_hash === input.request_hash;
      return { kind: exact ? "reused" : "conflict", record: existing };
    }
    const record: ExecutionRecord = {
      ...input,
      status: "running",
      created_at_ms: input.now_ms,
      updated_at_ms: input.now_ms,
    };
    this.rows.set(input.invocation_id, record);
    return { kind: "created", record };
  }

  async get(invocation_id: string): Promise<ExecutionRecord | null> {
    return this.rows.get(invocation_id) ?? null;
  }

  async finish(input: FinishExecutionInput): Promise<boolean> {
    const current = this.rows.get(input.invocation_id);
    if (
      current === undefined ||
      current.status !== "running" ||
      current.request_hash !== input.request_hash ||
      current.owner_token !== input.owner_token
    ) {
      return false;
    }
    this.rows.set(input.invocation_id, {
      ...current,
      status: input.status,
      artifact_ref: input.artifact_ref,
      updated_at_ms: input.now_ms,
    });
    return true;
  }
}

class MemoryArtifacts implements ExecutionArtifactStore {
  readonly values = new Map<string, ExecutionArtifact>();
  writes = 0;

  async write(value: ExecutionArtifact): Promise<ExecutionArtifactRef> {
    this.writes += 1;
    const key = `executions/${value.invocation_id}.json`;
    this.values.set(key, structuredClone(value));
    return {
      key,
      media_type: "application/json",
      bytes: JSON.stringify(value).length,
      sha256: `sha256:${"d".repeat(64)}`,
    };
  }

  async read(ref: ExecutionArtifactRef): Promise<ExecutionArtifact> {
    const value = this.values.get(ref.key);
    if (value === undefined) throw new Error("missing artifact");
    return structuredClone(value);
  }
}

function deferred<T>(): {
  readonly promise: Promise<T>;
  readonly resolve: (value: T) => void;
} {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
