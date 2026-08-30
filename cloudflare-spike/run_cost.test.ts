import assert from "node:assert/strict";
import { test } from "node:test";
import type { ExecutionArtifact } from "./src/execution_artifacts.ts";
import {
  RunCostMeter,
  sealArtifactCostBytes,
  WorkersAnalyticsEngineRunCost,
} from "./src/run_cost.ts";

test("cost meter preserves raw usage units and planned terminal operations", () => {
  const meter = new RunCostMeter();
  meter.observeRelational({ rows_read: 1, rows_written: 1 });
  meter.observeObject({ reads: 0, writes: 0, bytes_read: 0, bytes_written: 0 });
  meter.observeEvidence([
    {
      type: "usage",
      messageId: "m1",
      inputTokens: 120,
      outputTokens: 30,
      cacheReadInputTokens: 80,
      cacheCreationInputTokens: 10,
      costUsd: 0.0125,
      source: "native",
    },
  ]);
  const cost = meter.terminal("completed", {
    sandbox_duration_ms: 2_500,
    sandbox_profile: "standard-2",
    planned_d1_terminal_writes: 1,
    planned_r2_writes: 1,
    planned_analytics_points: 1,
  });

  assert.deepEqual(cost.model, {
    input_tokens: 120,
    output_tokens: 30,
    cache_read_input_tokens: 80,
    cache_creation_input_tokens: 10,
    provider_reported_cost_usd: 0.0125,
  });
  assert.equal(cost.infrastructure.d1_rows_read, 1);
  assert.equal(cost.infrastructure.d1_rows_written, 2);
  assert.equal(cost.infrastructure.r2_writes, 1);
  assert.equal(cost.infrastructure.analytics_points_written, 1);
  assert.equal(cost.known_cost_usd, 0.0125);
  assert.equal(cost.estimated_total_cost_usd, null);
  assert.equal(cost.rate_card_version, null);
});

test("artifact cost sealing converges on its exact serialized byte count", () => {
  const meter = new RunCostMeter();
  const artifact: ExecutionArtifact = {
    version: 1,
    invocation_id: "invocation-1",
    request_hash: `sha256:${"a".repeat(64)}`,
    terminal_result: { status: "completed" },
    evidence: [],
    cost: meter.terminal("completed", {
      sandbox_duration_ms: 1,
      sandbox_profile: null,
      planned_d1_terminal_writes: 1,
      planned_r2_writes: 1,
      planned_analytics_points: 1,
    }) as unknown as ExecutionArtifact["cost"],
  };
  const sealed = sealArtifactCostBytes(artifact);
  const bytes = new TextEncoder().encode(JSON.stringify(sealed)).byteLength;
  assert.equal(
    (sealed.cost as any).infrastructure.r2_bytes_written,
    bytes,
  );
});

test("analytics emits one compact point without run content", () => {
  const points: unknown[] = [];
  const analytics = new WorkersAnalyticsEngineRunCost({
    writeDataPoint(point: unknown) {
      points.push(point);
    },
  } as AnalyticsEngineDataset);
  const meter = new RunCostMeter();
  const cost = meter.terminal("failed", {
    sandbox_duration_ms: 10,
    sandbox_profile: "standard-2",
    planned_analytics_points: 1,
  });
  analytics.emit({
    invocation_id: "invocation-secret-not-emitted",
    request_hash: `sha256:${"b".repeat(64)}`,
    harness: "opencode",
    transport: "http",
    cost,
  });

  assert.equal(points.length, 1);
  const serialized = JSON.stringify(points[0]);
  assert.doesNotMatch(serialized, /invocation-secret-not-emitted/);
  assert.doesNotMatch(serialized, /prompt|output|repository/i);
  assert.match(serialized, /sha256:/);
});
