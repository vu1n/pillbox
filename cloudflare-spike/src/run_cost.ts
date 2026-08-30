import type { InvocationRequestHash, JsonValue } from "./codex_execution.js";
import type { ExecutionArtifact } from "./execution_artifacts.js";
import type { RelationalUsage } from "./execution_store.js";
import type { ObjectUsage } from "./execution_artifacts.js";
import type { Payload } from "./contract.js";

export interface ModelUsageCost {
  readonly input_tokens: number;
  readonly output_tokens: number;
  readonly cache_read_input_tokens: number;
  readonly cache_creation_input_tokens: number;
  readonly provider_reported_cost_usd: number | null;
}

export interface InfrastructureUsageCost {
  readonly d1_rows_read: number;
  readonly d1_rows_written: number;
  readonly r2_reads: number;
  readonly r2_writes: number;
  readonly r2_bytes_read: number;
  readonly r2_bytes_written: number;
  readonly analytics_points_written: number;
  readonly sandbox_duration_ms: number;
  readonly sandbox_profile: string | null;
}

export interface RunCostEnvelope {
  readonly version: 1;
  readonly status: "completed" | "failed" | "cancelled" | "interrupted";
  readonly model: ModelUsageCost;
  readonly infrastructure: InfrastructureUsageCost;
  readonly known_cost_usd: number | null;
  readonly estimated_total_cost_usd: number | null;
  readonly rate_card_version: string | null;
}

export interface RunCostAnalyticsPoint {
  readonly invocation_id: string;
  readonly request_hash: InvocationRequestHash;
  readonly harness: string;
  readonly transport: string;
  readonly cost: RunCostEnvelope;
}

export interface RunCostAnalytics {
  emit(point: RunCostAnalyticsPoint): Promise<void> | void;
}

/** Exact logical usage counters for one request-scoped execution service. */
export class RunCostMeter {
  private inputTokens = 0;
  private outputTokens = 0;
  private cacheReadTokens = 0;
  private cacheCreationTokens = 0;
  private providerCostUsd = 0;
  private hasProviderCost = false;
  private d1RowsRead = 0;
  private d1RowsWritten = 0;
  private r2Reads = 0;
  private r2Writes = 0;
  private r2BytesRead = 0;
  private r2BytesWritten = 0;

  readonly observeRelational = (usage: RelationalUsage): void => {
    this.d1RowsRead += usage.rows_read;
    this.d1RowsWritten += usage.rows_written;
  };

  readonly observeObject = (usage: ObjectUsage): void => {
    this.r2Reads += usage.reads;
    this.r2Writes += usage.writes;
    this.r2BytesRead += usage.bytes_read;
    this.r2BytesWritten += usage.bytes_written;
  };

  observeEvidence(events: readonly JsonValue[]): void {
    for (const event of events) {
      const payload = event as unknown as Payload;
      if (payload.type !== "usage") continue;
      this.inputTokens += finiteNonNegative(payload.inputTokens);
      this.outputTokens += finiteNonNegative(payload.outputTokens);
      this.cacheReadTokens += finiteNonNegative(payload.cacheReadInputTokens);
      this.cacheCreationTokens += finiteNonNegative(
        payload.cacheCreationInputTokens,
      );
      if (typeof payload.costUsd === "number" && Number.isFinite(payload.costUsd)) {
        this.providerCostUsd += Math.max(0, payload.costUsd);
        this.hasProviderCost = true;
      }
    }
  }

  terminal(
    status: RunCostEnvelope["status"],
    options: {
      readonly sandbox_duration_ms: number;
      readonly sandbox_profile: string | null;
      readonly planned_d1_terminal_writes?: number;
      readonly planned_r2_writes?: number;
      readonly planned_analytics_points?: number;
    },
  ): RunCostEnvelope {
    const known = this.hasProviderCost ? this.providerCostUsd : null;
    return {
      version: 1,
      status,
      model: {
        input_tokens: this.inputTokens,
        output_tokens: this.outputTokens,
        cache_read_input_tokens: this.cacheReadTokens,
        cache_creation_input_tokens: this.cacheCreationTokens,
        provider_reported_cost_usd: known,
      },
      infrastructure: {
        d1_rows_read: this.d1RowsRead,
        d1_rows_written:
          this.d1RowsWritten + (options.planned_d1_terminal_writes ?? 0),
        r2_reads: this.r2Reads,
        r2_writes: this.r2Writes + (options.planned_r2_writes ?? 0),
        r2_bytes_read: this.r2BytesRead,
        r2_bytes_written: this.r2BytesWritten,
        analytics_points_written: options.planned_analytics_points ?? 0,
        sandbox_duration_ms: Math.max(0, options.sandbox_duration_ms),
        sandbox_profile: options.sandbox_profile,
      },
      known_cost_usd: known,
      // Infrastructure prices change independently. A total is absent until a
      // versioned rate card is supplied; never present provider spend as total.
      estimated_total_cost_usd: null,
      rate_card_version: null,
    };
  }
}

/** Resolve the self-referential artifact byte count to a stable exact value. */
export function sealArtifactCostBytes(artifact: ExecutionArtifact): ExecutionArtifact {
  if (!isRunCostEnvelope(artifact.cost)) return artifact;
  let bytes = artifact.cost.infrastructure.r2_bytes_written;
  let current = artifact;
  for (let attempt = 0; attempt < 8; attempt++) {
    current = withArtifactBytes(current, bytes);
    const measured = new TextEncoder().encode(JSON.stringify(current)).byteLength;
    if (measured === bytes) return current;
    bytes = measured;
  }
  throw new Error("execution artifact byte-cost envelope did not converge");
}

export class WorkersAnalyticsEngineRunCost implements RunCostAnalytics {
  private readonly dataset: AnalyticsEngineDataset;

  constructor(dataset: AnalyticsEngineDataset) {
    this.dataset = dataset;
  }

  emit(point: RunCostAnalyticsPoint): void {
    const infra = point.cost.infrastructure;
    const model = point.cost.model;
    this.dataset.writeDataPoint({
      indexes: [point.request_hash],
      blobs: [
        point.cost.status,
        point.harness,
        point.transport,
        point.cost.rate_card_version ?? "unpriced",
      ],
      doubles: [
        model.input_tokens,
        model.output_tokens,
        model.cache_read_input_tokens,
        model.cache_creation_input_tokens,
        model.provider_reported_cost_usd ?? -1,
        infra.d1_rows_read,
        infra.d1_rows_written,
        infra.r2_reads,
        infra.r2_writes,
        infra.r2_bytes_read,
        infra.r2_bytes_written,
        infra.analytics_points_written,
        infra.sandbox_duration_ms,
      ],
    });
  }
}

function finiteNonNegative(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value)
    ? Math.max(0, value)
    : 0;
}

function isRunCostEnvelope(value: unknown): value is RunCostEnvelope {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { version?: unknown }).version === 1 &&
    typeof (value as { infrastructure?: unknown }).infrastructure === "object"
  );
}

function withArtifactBytes(
  artifact: ExecutionArtifact,
  bytes: number,
): ExecutionArtifact {
  const cost = artifact.cost as unknown as RunCostEnvelope;
  return {
    ...artifact,
    cost: {
      ...cost,
      infrastructure: {
        ...cost.infrastructure,
        r2_bytes_written: bytes,
      },
    } as unknown as JsonValue,
  };
}
