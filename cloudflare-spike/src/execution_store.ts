import type {
  ExecutionArtifactRef,
  ExecutionAttribution,
  ExecutionDigest,
  InvocationRequestHash,
} from "./codex_execution.js";

export type ExecutionStatus =
  | "running"
  | "completed"
  | "failed"
  | "cancelled"
  | "interrupted";

export interface ExecutionRecord {
  readonly invocation_id: string;
  readonly idempotency_key: string;
  readonly request_hash: InvocationRequestHash;
  readonly execution_digest: ExecutionDigest;
  readonly execution_policy_revision: string;
  readonly session_id: string;
  readonly attribution: ExecutionAttribution;
  readonly status: ExecutionStatus;
  readonly owner_token: string;
  readonly lease_expires_at_ms: number;
  readonly created_at_ms: number;
  readonly updated_at_ms: number;
  readonly artifact_ref?: ExecutionArtifactRef;
}

export interface ExecutionClaimInput {
  readonly invocation_id: string;
  readonly idempotency_key: string;
  readonly request_hash: InvocationRequestHash;
  readonly execution_digest: ExecutionDigest;
  readonly execution_policy_revision: string;
  readonly session_id: string;
  readonly attribution: ExecutionAttribution;
  readonly owner_token: string;
  readonly now_ms: number;
  readonly lease_expires_at_ms: number;
}

export const MAX_MANAGED_EXECUTION_LIMIT = 1_000;

export interface ManagedExecutionAllowance {
  readonly deployment_epoch: string;
  readonly execution_limit: number;
}

export interface ManagedExecutionAllowanceSnapshot extends ManagedExecutionAllowance {
  readonly reserved_executions: number;
}

export class ManagedExecutionAllowanceError extends Error {
  readonly code = "managed_disabled" as const;

  constructor() {
    super(
      "Pillbox managed execution allowance is exhausted or does not match the deployment configuration",
    );
    this.name = "ManagedExecutionAllowanceError";
  }
}

export function parseManagedExecutionAllowance(
  deploymentEpoch: string | undefined,
  executionLimit: string | undefined,
): ManagedExecutionAllowance | null {
  if (
    deploymentEpoch === undefined ||
    !/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(deploymentEpoch) ||
    executionLimit === undefined ||
    !/^[1-9][0-9]*$/.test(executionLimit)
  ) {
    return null;
  }
  const parsedLimit = Number(executionLimit);
  if (
    !Number.isSafeInteger(parsedLimit) ||
    parsedLimit > MAX_MANAGED_EXECUTION_LIMIT
  ) {
    return null;
  }
  return {
    deployment_epoch: deploymentEpoch,
    execution_limit: parsedLimit,
  };
}

export type ExecutionClaim =
  | { readonly kind: "created"; readonly record: ExecutionRecord }
  | { readonly kind: "reused"; readonly record: ExecutionRecord }
  | { readonly kind: "conflict"; readonly record: ExecutionRecord };

export interface FinishExecutionInput {
  readonly invocation_id: string;
  readonly request_hash: InvocationRequestHash;
  readonly owner_token: string;
  readonly status: Exclude<ExecutionStatus, "running">;
  readonly artifact_ref: ExecutionArtifactRef;
  readonly now_ms: number;
}

export interface ExecutionStore {
  claim(
    input: ExecutionClaimInput,
    allowance: ManagedExecutionAllowance,
  ): Promise<ExecutionClaim>;
  get(invocation_id: string): Promise<ExecutionRecord | null>;
  getAllowance(
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedExecutionAllowanceSnapshot | null>;
  finish(input: FinishExecutionInput): Promise<boolean>;
}

export interface RelationalResult<T = unknown> {
  readonly results?: readonly T[];
  readonly meta?: {
    readonly changes?: number;
    readonly rows_read?: number;
    readonly rows_written?: number;
  };
}

export interface RelationalStatement {
  bind(...values: readonly unknown[]): RelationalStatement;
  all<T>(): Promise<RelationalResult<T>>;
  run(): Promise<RelationalResult>;
}

export interface RelationalDatabase {
  prepare(sql: string): RelationalStatement;
}

export interface RelationalUsage {
  readonly rows_read: number;
  readonly rows_written: number;
}

type UsageObserver = (usage: RelationalUsage) => void;

interface ExecutionRow {
  invocation_id: string;
  idempotency_key: string;
  request_hash: InvocationRequestHash;
  execution_digest: ExecutionDigest;
  execution_policy_revision: string;
  session_id: string;
  harness: ExecutionAttribution["harness"];
  transport: string;
  requested_model: string;
  status: ExecutionStatus;
  owner_token: string;
  lease_expires_at_ms: number;
  created_at_ms: number;
  updated_at_ms: number;
  artifact_key: string | null;
  artifact_media_type: string | null;
  artifact_bytes: number | null;
  artifact_sha256: InvocationRequestHash | null;
}

const SELECT_COLUMNS = `
  invocation_id, idempotency_key, request_hash, execution_digest,
  execution_policy_revision, session_id, harness, transport, requested_model,
  status, owner_token,
  lease_expires_at_ms, created_at_ms, updated_at_ms,
  artifact_key, artifact_media_type, artifact_bytes, artifact_sha256
`;

export class D1ExecutionStore implements ExecutionStore {
  private readonly database: RelationalDatabase;
  private readonly observeUsage: UsageObserver;

  constructor(
    database: RelationalDatabase,
    observeUsage: UsageObserver = () => {},
  ) {
    this.database = database;
    this.observeUsage = observeUsage;
  }

  async claim(
    input: ExecutionClaimInput,
    allowance: ManagedExecutionAllowance,
  ): Promise<ExecutionClaim> {
    const byInvocation = await this.get(input.invocation_id);
    if (byInvocation !== null) return classifyClaim(byInvocation, input);

    let inserted: RelationalResult;
    try {
      inserted = await this.run(
        `INSERT OR IGNORE INTO execution (
        invocation_id, idempotency_key, request_hash, execution_digest,
        execution_policy_revision, session_id, harness, transport,
        requested_model, status, owner_token,
        lease_expires_at_ms, created_at_ms, updated_at_ms,
        allowance_epoch, allowance_limit
      )
      SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', ?, ?, ?, ?, ?, ?
      WHERE EXISTS (
        SELECT 1 FROM managed_execution_allowance
        WHERE singleton = 1 AND deployment_epoch = ? AND execution_limit = ?
          AND reserved_executions < execution_limit
        LIMIT 1
      )`,
        [
          input.invocation_id,
          input.idempotency_key,
          input.request_hash,
          input.execution_digest,
          input.execution_policy_revision,
          input.session_id,
          input.attribution.harness,
          input.attribution.transport,
          input.attribution.requested_model,
          input.owner_token,
          input.lease_expires_at_ms,
          input.now_ms,
          input.now_ms,
          allowance.deployment_epoch,
          allowance.execution_limit,
          allowance.deployment_epoch,
          allowance.execution_limit,
        ],
      );
    } catch (cause) {
      if (String(cause).includes("managed_execution_allowance_unavailable")) {
        throw new ManagedExecutionAllowanceError();
      }
      throw cause;
    }
    // D1 may include the trigger's reservation update in `changes`; zero is the
    // only ignored/no-capacity outcome.
    if ((inserted.meta?.changes ?? 0) > 0) {
      return { kind: "created", record: recordFromClaim(input) };
    }

    // A concurrent claim or the unique idempotency key won. Both lookups are
    // indexed and bounded to one row; there is deliberately no scan fallback.
    const winner =
      (await this.get(input.invocation_id)) ??
      (await this.getByIdempotencyKey(input.idempotency_key));
    if (winner === null) {
      throw new ManagedExecutionAllowanceError();
    }
    return classifyClaim(winner, input);
  }

  async get(invocation_id: string): Promise<ExecutionRecord | null> {
    return this.queryOne(
      `SELECT ${SELECT_COLUMNS} FROM execution WHERE invocation_id = ? LIMIT 1`,
      [invocation_id],
    );
  }

  async getAllowance(
    allowance: ManagedExecutionAllowance,
  ): Promise<ManagedExecutionAllowanceSnapshot | null> {
    const result = await this.database
      .prepare(
        `SELECT deployment_epoch, execution_limit, reserved_executions
         FROM managed_execution_allowance
         WHERE singleton = 1 AND deployment_epoch = ? AND execution_limit = ?
         LIMIT 1`,
      )
      .bind(allowance.deployment_epoch, allowance.execution_limit)
      .all<ManagedExecutionAllowanceSnapshot>();
    this.observe(result);
    const rows = result.results ?? [];
    if (rows.length > 1) throw new Error("indexed allowance query returned multiple rows");
    return rows[0] ?? null;
  }

  async finish(input: FinishExecutionInput): Promise<boolean> {
    const result = await this.run(
      `UPDATE execution SET
        status = ?, artifact_key = ?, artifact_media_type = ?,
        artifact_bytes = ?, artifact_sha256 = ?, updated_at_ms = ?
      WHERE invocation_id = ? AND request_hash = ? AND owner_token = ?
        AND status = 'running'`,
      [
        input.status,
        input.artifact_ref.key,
        input.artifact_ref.media_type,
        input.artifact_ref.bytes,
        input.artifact_ref.sha256,
        input.now_ms,
        input.invocation_id,
        input.request_hash,
        input.owner_token,
      ],
    );
    return (result.meta?.changes ?? 0) === 1;
  }

  private async getByIdempotencyKey(
    idempotency_key: string,
  ): Promise<ExecutionRecord | null> {
    return this.queryOne(
      `SELECT ${SELECT_COLUMNS} FROM execution WHERE idempotency_key = ? LIMIT 1`,
      [idempotency_key],
    );
  }

  private async queryOne(
    sql: string,
    values: readonly unknown[],
  ): Promise<ExecutionRecord | null> {
    const result = await this.database.prepare(sql).bind(...values).all<ExecutionRow>();
    this.observe(result);
    const rows = result.results ?? [];
    if (rows.length > 1) throw new Error("indexed execution query returned multiple rows");
    return rows.length === 0 ? null : recordFromRow(rows[0]);
  }

  private async run(
    sql: string,
    values: readonly unknown[],
  ): Promise<RelationalResult> {
    const result = await this.database.prepare(sql).bind(...values).run();
    this.observe(result);
    return result;
  }

  private observe(result: RelationalResult): void {
    this.observeUsage({
      rows_read: result.meta?.rows_read ?? 0,
      rows_written: result.meta?.rows_written ?? 0,
    });
  }
}

function classifyClaim(
  record: ExecutionRecord,
  input: ExecutionClaimInput,
): ExecutionClaim {
  const exact =
    record.invocation_id === input.invocation_id &&
    record.idempotency_key === input.idempotency_key &&
    record.request_hash === input.request_hash;
  return { kind: exact ? "reused" : "conflict", record };
}

function recordFromClaim(input: ExecutionClaimInput): ExecutionRecord {
  return {
    invocation_id: input.invocation_id,
    idempotency_key: input.idempotency_key,
    request_hash: input.request_hash,
    execution_digest: input.execution_digest,
    execution_policy_revision: input.execution_policy_revision,
    session_id: input.session_id,
    attribution: input.attribution,
    status: "running",
    owner_token: input.owner_token,
    lease_expires_at_ms: input.lease_expires_at_ms,
    created_at_ms: input.now_ms,
    updated_at_ms: input.now_ms,
  };
}

function recordFromRow(row: ExecutionRow): ExecutionRecord {
  const artifact_ref =
    row.artifact_key === null ||
    row.artifact_media_type !== "application/json" ||
    row.artifact_bytes === null ||
    row.artifact_sha256 === null
      ? undefined
      : {
          key: row.artifact_key,
          media_type: row.artifact_media_type,
          bytes: row.artifact_bytes,
          sha256: row.artifact_sha256,
        } as const;
  return {
    invocation_id: row.invocation_id,
    idempotency_key: row.idempotency_key,
    request_hash: row.request_hash,
    execution_digest: row.execution_digest,
    execution_policy_revision: row.execution_policy_revision,
    session_id: row.session_id,
    attribution: {
      harness: row.harness,
      transport: row.transport,
      requested_model: row.requested_model,
      served_model: null,
    },
    status: row.status,
    owner_token: row.owner_token,
    lease_expires_at_ms: row.lease_expires_at_ms,
    created_at_ms: row.created_at_ms,
    updated_at_ms: row.updated_at_ms,
    ...(artifact_ref === undefined ? {} : { artifact_ref }),
  };
}
