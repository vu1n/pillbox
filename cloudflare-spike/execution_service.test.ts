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
import { ExecutionArtifactConflictError } from "./src/execution_artifacts.ts";
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
import type { ManagedExecutionOwner } from "./src/managed_ownership.ts";
import {
  ManagedExecutionAllowanceError,
  type ManagedExecutionAllowance,
  type ManagedExecutionReservation,
  type ManagedExecutionReservationStore,
  type ManagedReservationInput,
  type ManagedReservationClaim,
} from "./src/managed_reservation.ts";
import {
  RunCostMeter,
  type RunCostAnalyticsPoint,
} from "./src/run_cost.ts";
import { managedAdmissionPolicy } from "./src/managed_admission.ts";

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

const allowance: ManagedExecutionAllowance = {
  deployment_epoch: "preview-2026-09-01",
  execution_limit: 3,
};
const executionOwner: ManagedExecutionOwner = {
  domain: "huddles_workspace",
  digest: `sha256:${"d".repeat(64)}`,
};

async function request(
  changes: Partial<ExecuteInvocationV2Request> = {},
): Promise<ExecuteInvocationV2Request> {
  const rendered_input = changes.rendered_input ?? "Produce a JSON result.";
  return {
    contract_version: "pillbox.execution/2",
    session_ref: { session_id: "session-1" },
    invocation_id: "invocation-1",
    idempotency_key: "invocation-1",
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
  const terminalOrder: string[] = [];
  const store = new MemoryStore(() => terminalOrder.push("d1"));
  const artifacts = new MemoryArtifacts(() => terminalOrder.push("r2"));
  const analytics: RunCostAnalyticsPoint[] = [];
  const runtime = new FakeRuntime({
    served_model: "zai-coding-plan/glm-4.5-air",
    output: { json: { ok: true } },
    evidence: [
      { type: "message_start", messageId: "m1" },
      { type: "message_delta", messageId: "m1", text: "done" },
      {
        type: "usage",
        messageId: "m1",
        inputTokens: 10,
        outputTokens: 2,
        costUsd: 0.001,
        source: "native",
      },
    ],
  });
  let authorizationChecks = 0;
  const service = new ExecutionService(store, artifacts, runtime, {
    ...fixedOptions(),
    costMeter: new RunCostMeter(),
    analytics: {
      emit: (point) => {
        terminalOrder.push("analytics");
        analytics.push(point);
      },
    },
    sandboxProfile: "standard-2",
    authorizer: async () => {
      authorizationChecks += 1;
      return executionOwner;
    },
  });
  const input = await request();

  const created = await service.executeInvocation(input);
  assert.equal(created.status, "completed");
  assert.equal(created.disposition, "created");
  assert.equal(created.attribution.harness, "opencode");
  assert.equal(created.evidence.events.length, 3);
  assert.deepEqual(created.session_ref.seq_range, [0, 2]);
  assert.ok(created.evidence.artifact_ref);
  assert.equal(created.cost?.model.provider_reported_cost_usd, 0.001);
  assert.equal(created.cost?.infrastructure.analytics_points_planned, 1);
  assert.deepEqual(terminalOrder, ["r2", "d1", "analytics"]);

  const reused = await service.executeInvocation(input);
  assert.equal(reused.status, "completed");
  assert.equal(reused.disposition, "reused");
  assert.equal(runtime.executions, 1);
  assert.equal(artifacts.writes, 1);
  assert.equal(analytics.length, 1);
  assert.equal(authorizationChecks, 2, "exact retry is reauthorized before D1 reuse");
});

test("zero-event runtime success becomes a typed terminal failure", async () => {
  const service = new ExecutionService(
    new MemoryStore(),
    new MemoryArtifacts(),
    new FakeRuntime({
      served_model: "zai-coding-plan/glm-4.5-air",
      output: { text: "unattested output" },
      evidence: [],
    }),
    {
      ...fixedOptions(),
      costMeter: new RunCostMeter(),
    },
  );

  const input = await request();
  const result = await service.executeInvocation(input);
  assert.equal(result.status, "failed");
  if (result.status === "failed") {
    assert.equal(result.error.code, "runtime_failed");
    assert.match(result.error.message, /without immutable positional evidence/);
  }
  assert.equal(result.session_ref.seq_range, undefined);
  assert.deepEqual(result.evidence.events, []);
  assert.equal(result.cost?.status, "failed");
  assert.equal(result.cost?.model.input_tokens, 0);
  assert.equal(result.cost?.model.output_tokens, 0);
  assert.equal(result.cost?.infrastructure.analytics_points_planned, 0);

  const reused = await service.executeInvocation(input);
  assert.equal(reused.status, "failed");
  assert.equal(reused.disposition, "reused");
  assert.equal(reused.session_ref.seq_range, undefined);
});

test("execute, status, and cancel authorization failures precede every persistence access", async () => {
  const operations: string[] = [];
  const never = (): never => {
    throw new Error("authorization failure crossed into persistence");
  };
  const service = new ExecutionService(
    { claim: async () => never(), get: async () => never(), finish: async () => never() },
    { write: async () => never(), read: async () => never() },
    { execute: async () => never(), cancel: async () => never() },
    {
      ...fixedOptions(),
      authorizer: async ({ operation }) => {
        operations.push(operation);
        throw new Error(`denied ${operation}`);
      },
    },
  );
  const input = await request();

  await assert.rejects(service.executeInvocation(input), /denied execute/);
  await assert.rejects(
    service.getExecutionStatus({
      contract_version: "pillbox.execution/2",
      invocation_id: input.invocation_id,
      evidence_after: 0,
      evidence_limit: 100,
    }),
    /denied status/,
  );
  await assert.rejects(
    service.cancelInvocation({
      contract_version: "pillbox.execution/2",
      invocation_id: input.invocation_id,
      idempotency_key: input.invocation_id,
      reason: "test",
    }),
    /denied cancel/,
  );
  assert.deepEqual(operations, ["execute", "status", "cancel"]);
});

test("tool-enabled managed execution fails closed before runtime access", async () => {
  let claims = 0;
  const runtime = new FakeRuntime({
    served_model: null,
    output: { text: "must not run" },
    evidence: [],
  });
  const service = new ExecutionService(
    {
      claim: async () => {
        claims += 1;
        throw new Error("unsupported policy reached D1 claim");
      },
      get: async () => null,
      finish: async () => false,
    },
    new MemoryArtifacts(),
    runtime,
    fixedOptions(),
  );
  const result = await service.executeInvocation(
    await request({ tool_policy: "runtime_default" }),
  );
  assert.equal(result.status, "failed");
  if (result.status === "failed") {
    assert.equal(result.error.code, "unsupported_policy");
  }
  assert.equal(runtime.executions, 0);
  assert.equal(claims, 0);
});

test("unsupported managed transport rejects repeatably before allowance reservation", async () => {
  let claims = 0;
  const supported = await request();
  const service = new ExecutionService(
    {
      claim: async () => {
        claims += 1;
        throw new Error("unsupported execution reached D1 claim");
      },
      get: async () => null,
      finish: async () => false,
    },
    new MemoryArtifacts(),
    new FakeRuntime({ served_model: null, evidence: [] }),
    fixedOptions(),
  );
  const input = await request({
    execution: {
      ...supported.execution,
      transport: {
        ...supported.execution.transport,
        harness: "codex",
        transport: "app_server",
      },
    },
  });

  const first = await service.executeInvocation(input);
  const retry = await service.executeInvocation(input);
  assert.deepEqual(retry, first);
  assert.equal(first.status, "failed");
  if (first.status === "failed") {
    assert.equal(first.error.code, "unsupported_execution");
  }
  assert.equal(claims, 0);
});

test("disabled managed execution returns a typed failure before charged access", async () => {
  const never = (): never => {
    throw new Error("disabled admission crossed into a charged dependency");
  };
  let authorized = false;
  const service = new ExecutionService(
    {
      claim: async () => never(),
      get: async () => never(),
      finish: async () => never(),
    },
    {
      write: async () => never(),
      read: async () => never(),
    },
    {
      execute: async () => never(),
      cancel: async () => never(),
    },
    {
      now: never,
      ownerToken: never,
      analytics: { emit: async () => never() },
      admission: managedAdmissionPolicy(undefined),
      reservations: new MemoryReservations(),
      authorizer: async () => {
        authorized = true;
        return executionOwner;
      },
    },
  );

  const result = await service.executeInvocation(await request());
  assert.equal(result.status, "failed");
  if (result.status === "failed") {
    assert.equal(result.error.code, "managed_disabled");
  }
  assert.equal(result.session_ref.seq_range, undefined);
  assert.deepEqual(result.evidence.events, []);
  assert.equal(authorized, true, "managed-disabled drain happens only after authorization");
});

test("enabled managed execution with missing allowance config fails before charged access", async () => {
  const never = (): never => {
    throw new Error("missing allowance crossed into a charged dependency");
  };
  const service = new ExecutionService(
    {
      claim: async () => never(),
      get: async () => never(),
      finish: async () => never(),
    },
    {
      write: async () => never(),
      read: async () => never(),
    },
    {
      execute: async () => never(),
      cancel: async () => never(),
    },
    {
      now: never,
      ownerToken: never,
      analytics: { emit: async () => never() },
      admission: managedAdmissionPolicy("1"),
      reservations: new MemoryReservations(),
      authorizer: async () => executionOwner,
    },
  );

  const result = await service.executeInvocation(await request());
  assert.equal(result.status, "failed");
  if (result.status === "failed") {
    assert.equal(result.error.code, "managed_disabled");
    assert.match(result.error.message, /allowance is not configured/);
  }
});

test("exhausted allowance fails before Sandbox, R2, or Analytics", async () => {
  const never = (): never => {
    throw new Error("exhausted allowance crossed into a charged dependency");
  };
  const service = new ExecutionService(
    {
      claim: async () => never(),
      get: async () => never(),
      finish: async () => never(),
    },
    {
      write: async () => never(),
      read: async () => never(),
    },
    {
      execute: async () => never(),
      cancel: async () => never(),
    },
    {
      ...fixedOptions(),
      reservations: {
        ...new MemoryReservations(),
        claimExecution: async () => {
          throw new ManagedExecutionAllowanceError();
        },
      },
      analytics: { emit: async () => never() },
    },
  );

  const result = await service.executeInvocation(await request());
  assert.equal(result.status, "failed");
  if (result.status === "failed") {
    assert.equal(result.error.code, "managed_disabled");
    assert.match(result.error.message, /allowance is exhausted/);
  }
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
  assert.equal(conflict.session_ref.seq_range, undefined);
  assert.equal(runtime.executions, 1);
});

test("failed Analytics emission is logged after terminal commit and never retried", async () => {
  const store = new MemoryStore();
  const artifacts = new MemoryArtifacts();
  const runtime = new FakeRuntime({
    served_model: "zai-coding-plan/glm-4.5-air",
    output: { text: "done" },
    evidence: [{ type: "message_delta", text: "done" }],
  });
  let emissionAttempts = 0;
  const warnings: unknown[][] = [];
  const originalWarn = console.warn;
  console.warn = (...args: unknown[]) => void warnings.push(args);
  const service = new ExecutionService(store, artifacts, runtime, {
    ...fixedOptions(),
    costMeter: new RunCostMeter(),
    analytics: {
      emit: async () => {
        emissionAttempts += 1;
        throw new Error("Analytics unavailable");
      },
    },
  });
  const input = await request();

  try {
    const completed = await service.executeInvocation(input);
    assert.equal(completed.status, "completed");
    assert.equal(completed.cost?.infrastructure.analytics_points_planned, 1);
    assert.equal(emissionAttempts, 1);
    assert.equal(artifacts.writes, 1);
    assert.equal(store.rows.get(input.invocation_id)?.status, "completed");
    assert.match(String(warnings[0]?.join(" ")), /Analytics unavailable/);

    const retry = await service.executeInvocation(input);
    assert.equal(retry.status, "completed");
    assert.equal(runtime.executions, 1, "exact retry must not resample after failed accounting");
    assert.equal(emissionAttempts, 1, "terminal D1 reuse must not emit a second point");
  } finally {
    console.warn = originalWarn;
  }
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
  assert.equal(retry.session_ref.seq_range, undefined);
  assert.equal(runtime.executions, 1);

  pending.resolve({
    served_model: null,
    output: { text: "done" },
    evidence: [{ type: "message_delta", text: "done" }],
  });
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
    ...fixedOptions(),
    now: () => EXECUTION_OWNER_LEASE_MS + 1,
    ownerToken: () => "unused-owner",
  });

  const result = await service.executeInvocation(input);
  assert.equal(result.status, "interrupted");
  assert.equal(runtime.executions, 0);
});

test("an immutable terminal artifact repairs a lost D1 terminal write", async () => {
  let now = 1_000;
  const store = new MemoryStore();
  store.finishFailures = 1;
  const artifacts = new MemoryArtifacts();
  const analytics: RunCostAnalyticsPoint[] = [];
  const service = new ExecutionService(
    store,
    artifacts,
    new FakeRuntime({
      served_model: "zai-coding-plan/glm-4.5-air",
      output: { text: "done" },
      evidence: [{ type: "message_delta", text: "done" }],
    }),
    {
      ...fixedOptions(),
      now: () => now,
      ownerToken: () => "owner-1",
      costMeter: new RunCostMeter(),
      analytics: { emit: (point) => analytics.push(point) },
    },
  );

  const first = await service.executeInvocation(await request());
  assert.equal(first.status, "running");
  assert.equal(analytics.length, 0, "a lost terminal CAS cannot emit Analytics");

  now += EXECUTION_OWNER_LEASE_MS + 1;
  const recovered = await service.getExecutionStatus({
    contract_version: "pillbox.execution/2",
    invocation_id: "invocation-1",
    evidence_after: 0,
    evidence_limit: 100,
  });
  assert.equal(recovered.status, "completed");
  assert.equal(recovered.attribution.served_model, "zai-coding-plan/glm-4.5-air");
  assert.equal(artifacts.values.size, 1);
  assert.equal(analytics.length, 1, "the terminal CAS winner emits once after recovery");
  assert.equal(
    (
      await service.getExecutionStatus({
        contract_version: "pillbox.execution/2",
        invocation_id: "invocation-1",
        evidence_after: 0,
        evidence_limit: 100,
      })
    ).status,
    "completed",
  );
  assert.equal(analytics.length, 1, "terminal status reuse cannot re-emit Analytics");
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
  assert.deepEqual(page.session_ref.seq_range, [0, 2]);
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
    idempotency_key: input.invocation_id,
    reason: "caller stopped the run",
  } as const;

  assert.equal((await service.cancelInvocation(cancel)).status, "cancelled");
  assert.equal((await service.cancelInvocation(cancel)).status, "cancelled");
  assert.equal(runtime.cancellations, 1);
});

test("foreign owners cannot read evidence, status, or cancel an invocation", async () => {
  const input = await request();
  const store = new MemoryStore();
  const artifacts = new MemoryArtifacts();
  const runtime = new FakeRuntime({
    served_model: null,
    output: { text: "done" },
    evidence: [{ type: "message_delta", text: "private" }],
  });
  await new ExecutionService(store, artifacts, runtime, fixedOptions()).executeInvocation(input);
  const foreignOwner: ManagedExecutionOwner = {
    domain: "huddles_workspace",
    digest: `sha256:${"e".repeat(64)}`,
  };
  const foreign = new ExecutionService(store, artifacts, runtime, {
    ...fixedOptions(),
    authorizer: async () => foreignOwner,
  });
  await assert.rejects(
    foreign.getExecutionStatus({
      contract_version: "pillbox.execution/2",
      invocation_id: input.invocation_id,
      evidence_after: 0,
      evidence_limit: 100,
    }),
    /was not found/,
  );
  await assert.rejects(
    foreign.cancelInvocation({
      contract_version: "pillbox.execution/2",
      invocation_id: input.invocation_id,
      idempotency_key: input.invocation_id,
      reason: "foreign",
    }),
    /was not found/,
  );
  assert.equal(artifacts.reads, 0);
  assert.equal(runtime.cancellations, 0);
});

test("disabled admission still permits status, cancellation, and terminal drain", async () => {
  const input = await request();
  const store = new MemoryStore();
  await seedRunning(store, input, 1_000);
  const runtime = new FakeRuntime({
    served_model: null,
    output: { text: "must not run" },
    evidence: [],
  });
  const service = new ExecutionService(store, new MemoryArtifacts(), runtime, {
    ...fixedOptions(),
    now: () => 1_000,
    ownerToken: () => "unused-owner",
    admission: managedAdmissionPolicy(undefined),
  });

  const running = await service.getExecutionStatus({
    contract_version: "pillbox.execution/2",
    invocation_id: input.invocation_id,
    evidence_after: 0,
    evidence_limit: 100,
  });
  assert.equal(running.status, "running");

  const cancelled = await service.cancelInvocation({
    contract_version: "pillbox.execution/2",
    invocation_id: input.invocation_id,
    idempotency_key: input.invocation_id,
    reason: "cost circuit breaker",
  });
  assert.equal(cancelled.status, "cancelled");
  assert.equal(cancelled.session_ref.seq_range, undefined);
  assert.equal(runtime.cancellations, 1);

  const terminal = await service.getExecutionStatus({
    contract_version: "pillbox.execution/2",
    invocation_id: input.invocation_id,
    evidence_after: 0,
    evidence_limit: 100,
  });
  assert.equal(terminal.status, "cancelled");
  assert.equal(terminal.session_ref.seq_range, undefined);
});

test("one evidence event produces one inclusive managed position", async () => {
  const service = new ExecutionService(
    new MemoryStore(),
    new MemoryArtifacts(),
    new FakeRuntime({
      served_model: null,
      output: { text: "done" },
      evidence: [{ type: "message_delta", text: "done" }],
    }),
    fixedOptions(),
  );

  const result = await service.executeInvocation(await request());
  assert.equal(result.status, "completed");
  assert.deepEqual(result.session_ref.seq_range, [0, 0]);
});

async function seedRunning(
  store: MemoryStore,
  input: ExecuteInvocationV2Request,
  now_ms: number,
): Promise<void> {
  await store.claim(
    {
      invocation_id: input.invocation_id,
      idempotency_key: input.idempotency_key,
      request_hash: await computeInvocationRequestHash(input),
      execution_digest: await computeExecutionIdentityDigest(
        input.execution,
        input.execution_policy_revision,
      ),
      execution_policy_revision: input.execution_policy_revision,
      session_id: input.session_ref.session_id,
      owner: executionOwner,
      allowance_epoch: allowance.deployment_epoch,
      allowance_limit: allowance.execution_limit,
      attribution: {
        harness: input.execution.transport.harness,
        transport: input.execution.transport.transport,
        requested_model: `${input.execution.requested.provider}/${input.execution.requested.model}`,
        served_model: null,
      },
      owner_token: "seed-owner",
      now_ms,
      lease_expires_at_ms: now_ms + EXECUTION_OWNER_LEASE_MS,
    },
  );
}

function fixedOptions() {
  return {
    now: () => 1_000,
    ownerToken: () => "owner-1",
    admission: managedAdmissionPolicy("1"),
    allowance,
    reservations: new MemoryReservations(),
    authorizer: async () => executionOwner,
  };
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

  async cancel(
    _request: CancelInvocationV2Request,
    _session_id: string,
  ): Promise<void> {
    this.cancellations += 1;
  }
}

class MemoryStore implements ExecutionStore {
  readonly rows = new Map<string, ExecutionRecord>();
  finishFailures = 0;
  private readonly onFinish: () => void;

  constructor(onFinish: () => void = () => {}) {
    this.onFinish = onFinish;
  }

  async claim(
    input: ExecutionClaimInput,
  ): Promise<ExecutionClaim> {
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

  async get(invocation_id: string, owner: ManagedExecutionOwner): Promise<ExecutionRecord | null> {
    const row = this.rows.get(invocation_id);
    return row?.owner.domain === owner.domain && row.owner.digest === owner.digest ? row : null;
  }

  async finish(input: FinishExecutionInput): Promise<boolean> {
    this.onFinish();
    if (this.finishFailures > 0) {
      this.finishFailures -= 1;
      return false;
    }
    const current = this.rows.get(input.invocation_id);
    if (
      current === undefined ||
      current.status !== "running" ||
      current.request_hash !== input.request_hash ||
      current.owner_token !== input.owner_token ||
      current.owner.domain !== input.owner.domain ||
      current.owner.digest !== input.owner.digest
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

class MemoryReservations implements ManagedExecutionReservationStore {
  readonly rows = new Map<string, ManagedExecutionReservation>();

  async claimProvision(
    input: ManagedReservationInput,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedReservationClaim> {
    return this.claim(input, allowance, "provisioning");
  }

  async claimExecution(
    input: Omit<ManagedReservationInput, "source" | "provision_request_digest">,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedReservationClaim> {
    const existing = this.rows.get(input.invocation_id);
    if (existing !== undefined) {
      const exact =
        existing.session_id === input.session_id &&
        existing.owner.domain === input.owner.domain &&
        existing.owner.digest === input.owner.digest &&
        existing.execution_request_hash === input.execution_request_hash &&
        existing.allowance_epoch === allowance.deployment_epoch &&
        existing.allowance_limit === allowance.execution_limit;
      return { kind: exact ? "reused" : "conflict", record: existing };
    }
    return this.claim({ ...input, source: "direct_execution" }, allowance, "ready");
  }

  async getReady(
    invocationId: string,
    sessionId: string,
    owner: ManagedExecutionOwner,
    requestHash: `sha256:${string}`,
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedExecutionReservation | null> {
    const row = this.rows.get(invocationId);
    return row?.status === "ready" && row.session_id === sessionId &&
      row.owner.domain === owner.domain && row.owner.digest === owner.digest &&
      row.execution_request_hash === requestHash &&
      row.allowance_epoch === allowance.deployment_epoch &&
      row.allowance_limit === allowance.execution_limit ? row : null;
  }

  async getSessionOwner(sessionId: string): Promise<ManagedExecutionOwner | null> {
    return [...this.rows.values()].find((row) => row.session_id === sessionId)?.owner ?? null;
  }

  async markReady(input: { invocation_id: string; owner: ManagedExecutionOwner; now_ms: number }): Promise<boolean> {
    return this.transition(input, "ready", null);
  }

  async markFailed(input: { invocation_id: string; owner: ManagedExecutionOwner; error_code: string; now_ms: number }): Promise<boolean> {
    return this.transition(input, "failed", input.error_code);
  }

  async getAllowance(config: ManagedExecutionAllowance) {
    return { ...config, reserved_executions: this.rows.size };
  }

  private async claim(
    input: ManagedReservationInput,
    config: ManagedExecutionAllowance,
    status: "provisioning" | "ready",
  ): Promise<ManagedReservationClaim> {
    const existing = this.rows.get(input.invocation_id);
    if (existing !== undefined) return { kind: "reused", record: existing };
    const record: ManagedExecutionReservation = {
      invocation_id: input.invocation_id,
      session_id: input.session_id,
      owner: input.owner,
      execution_request_hash: input.execution_request_hash,
      source: input.source,
      provision_request_digest: input.provision_request_digest ?? null,
      status,
      error_code: null,
      allowance_epoch: config.deployment_epoch,
      allowance_limit: config.execution_limit,
      created_at_ms: input.now_ms,
      updated_at_ms: input.now_ms,
    };
    this.rows.set(input.invocation_id, record);
    return { kind: "created", record };
  }

  private async transition(
    input: { invocation_id: string; owner: ManagedExecutionOwner; now_ms: number },
    status: "ready" | "failed",
    error_code: string | null,
  ): Promise<boolean> {
    const row = this.rows.get(input.invocation_id);
    if (row === undefined || row.owner.domain !== input.owner.domain || row.owner.digest !== input.owner.digest) return false;
    this.rows.set(input.invocation_id, { ...row, status, error_code, updated_at_ms: input.now_ms });
    return true;
  }
}

class MemoryArtifacts implements ExecutionArtifactStore {
  readonly values = new Map<string, ExecutionArtifact>();
  writes = 0;
  reads = 0;
  private readonly onWrite: () => void;

  constructor(onWrite: () => void = () => {}) {
    this.onWrite = onWrite;
  }

  async write(value: ExecutionArtifact): Promise<ExecutionArtifactRef> {
    this.onWrite();
    this.writes += 1;
    const key = `executions/${value.invocation_id}.json`;
    const existing = this.values.get(key);
    if (existing !== undefined) {
      const existingRef = memoryArtifactRef(key, existing);
      if (JSON.stringify(existing) === JSON.stringify(value)) return existingRef;
      throw new ExecutionArtifactConflictError(
        existingRef,
        structuredClone(existing),
      );
    }
    const ref = memoryArtifactRef(key, value);
    this.values.set(key, structuredClone(value));
    return ref;
  }

  async read(ref: ExecutionArtifactRef): Promise<ExecutionArtifact> {
    this.reads += 1;
    const value = this.values.get(ref.key);
    if (value === undefined) throw new Error("missing artifact");
    return structuredClone(value);
  }
}

function memoryArtifactRef(
  key: string,
  value: ExecutionArtifact,
): ExecutionArtifactRef {
  return {
    key,
    media_type: "application/json",
    bytes: JSON.stringify(value).length,
    sha256: `sha256:${"d".repeat(64)}`,
  };
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

test("provider admission runs after authorization and before reservations or allocation", async () => {
  const never = (): never => { throw new Error("provider rejection reached persistence"); };
  let authorized = false;
  const service = new ExecutionService(
    { claim: async () => never(), get: async () => never(), finish: async () => never() },
    { write: async () => never(), read: async () => never() },
    {
      preflight: () => { assert.equal(authorized, true); return { code: "managed_disabled", message: "DO disabled" }; },
      execute: async () => never(), cancel: async () => never(),
    },
    { ...fixedOptions(), authorizer: async () => { authorized = true; return executionOwner; } },
  );
  const input = await request();
  const result = await service.executeInvocation({ ...input, execution: { ...input.execution,
    transport: { ...input.execution.transport, transport: "digitalocean-sandbox-exec" } } });
  assert.equal(result.status, "failed");
  if (result.status === "failed") assert.equal(result.error.code, "managed_disabled");
});

test("provider cleanup receives only authorized expired or terminal execution records", async () => {
  const input = await request();
  const store = new MemoryStore();
  await seedRunning(store, input, 1);
  let cleanup = 0;
  const service = new ExecutionService(store, new MemoryArtifacts(), {
    execute: async () => { throw new Error("must not resample"); },
    cancel: async () => {},
    reconcile: async record => { cleanup++; assert.equal(record.invocation_id, input.invocation_id); },
  }, { ...fixedOptions(), now: () => EXECUTION_OWNER_LEASE_MS + 2 });
  const result = await service.getExecutionStatus({ contract_version: "pillbox.execution/2", invocation_id: input.invocation_id, evidence_after: 0, evidence_limit: 100 });
  assert.equal(result.status, "interrupted");
  assert.equal(cleanup, 1);
});

test("DO refuses to adopt a provisioned Cloudflare workspace before allocating runtime", async () => {
  const original = await request();
  const input = { ...original, execution: { ...original.execution, transport: { ...original.execution.transport, transport: "digitalocean-sandbox-exec" } } };
  const reservations = new MemoryReservations();
  await reservations.claimProvision({ invocation_id: input.invocation_id, session_id: input.session_ref.session_id,
    owner: executionOwner, execution_request_hash: await computeInvocationRequestHash(input), source: "workspace_provision",
    provision_request_digest: `sha256:${"f".repeat(64)}`, now_ms: 0 }, allowance);
  await reservations.markReady({ invocation_id: input.invocation_id, owner: executionOwner, now_ms: 1 });
  const runtime = new FakeRuntime({ served_model: null, output: { text: "must not run" }, evidence: [] });
  const service = new ExecutionService(new MemoryStore(), new MemoryArtifacts(), runtime, { ...fixedOptions(), reservations });
  const result = await service.executeInvocation(input);
  assert.equal(result.status, "failed");
  if (result.status === "failed") assert.equal(result.error.code, "unsupported_execution");
  assert.equal(runtime.executions, 0);
});
