import type {
  ExecutionArtifactRef,
  ExecutionAttribution,
  ExecutionDigest,
  InvocationRequestHash,
} from "./codex_execution.js";
import type { ManagedExecutionOwner } from "./managed_ownership.js";

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
  readonly owner: ManagedExecutionOwner;
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
  readonly owner: ManagedExecutionOwner;
  readonly allowance_epoch: string;
  readonly allowance_limit: number;
  readonly attribution: ExecutionAttribution;
  readonly owner_token: string;
  readonly now_ms: number;
  readonly lease_expires_at_ms: number;
}

export type ExecutionClaim =
  | { readonly kind: "created"; readonly record: ExecutionRecord }
  | { readonly kind: "reused"; readonly record: ExecutionRecord }
  | { readonly kind: "conflict"; readonly record: ExecutionRecord }
  | { readonly kind: "unavailable" };

export interface FinishExecutionInput {
  readonly invocation_id: string;
  readonly request_hash: InvocationRequestHash;
  readonly owner_token: string;
  readonly owner: ManagedExecutionOwner;
  readonly status: Exclude<ExecutionStatus, "running">;
  readonly artifact_ref: ExecutionArtifactRef;
  readonly now_ms: number;
}

export interface ExecutionStore {
  claim(
    input: ExecutionClaimInput,
  ): Promise<ExecutionClaim>;
  get(
    invocation_id: string,
    owner: ManagedExecutionOwner,
  ): Promise<ExecutionRecord | null>;
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
  owner_domain: ManagedExecutionOwner["domain"];
  owner_digest: `sha256:${string}`;
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
  owner_domain, owner_digest, status, owner_token,
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
  ): Promise<ExecutionClaim> {
    const byInvocation = await this.get(input.invocation_id, input.owner);
    if (byInvocation !== null) return classifyClaim(byInvocation, input);

    let inserted: RelationalResult;
    try {
      inserted = await this.run(
        `INSERT OR IGNORE INTO execution (
        invocation_id, idempotency_key, request_hash, execution_digest,
        execution_policy_revision, session_id, harness, transport,
        requested_model, owner_domain, owner_digest, status, owner_token,
        lease_expires_at_ms, created_at_ms, updated_at_ms,
        allowance_epoch, allowance_limit
      )
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', ?, ?, ?, ?, ?, ?)`,
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
          input.owner.domain,
          input.owner.digest,
          input.owner_token,
          input.lease_expires_at_ms,
          input.now_ms,
          input.now_ms,
          input.allowance_epoch,
          input.allowance_limit,
        ],
      );
    } catch (cause) {
      if (String(cause).includes("managed_execution_reservation_unavailable")) return { kind: "unavailable" };
      throw cause;
    }
    if ((inserted.meta?.changes ?? 0) > 0) {
      return { kind: "created", record: recordFromClaim(input) };
    }

    // A concurrent claim or the unique idempotency key won. Both lookups are
    // indexed and bounded to one row; there is deliberately no scan fallback.
    const winner =
      (await this.get(input.invocation_id, input.owner)) ??
      (await this.getByIdempotencyKey(input.idempotency_key, input.owner));
    if (winner === null) {
      return { kind: "unavailable" };
    }
    return classifyClaim(winner, input);
  }

  async get(
    invocation_id: string,
    owner: ManagedExecutionOwner,
  ): Promise<ExecutionRecord | null> {
    return this.queryOne(
      `SELECT ${SELECT_COLUMNS} FROM execution
       WHERE invocation_id = ? AND owner_domain = ? AND owner_digest = ? LIMIT 1`,
      [invocation_id, owner.domain, owner.digest],
    );
  }

  async finish(input: FinishExecutionInput): Promise<boolean> {
    const result = await this.run(
      `UPDATE execution SET
        status = ?, artifact_key = ?, artifact_media_type = ?,
        artifact_bytes = ?, artifact_sha256 = ?, updated_at_ms = ?
      WHERE invocation_id = ? AND request_hash = ? AND owner_token = ?
        AND owner_domain = ? AND owner_digest = ? AND status = 'running'`,
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
        input.owner.domain,
        input.owner.digest,
      ],
    );
    return (result.meta?.changes ?? 0) === 1;
  }

  private async getByIdempotencyKey(
    idempotency_key: string,
    owner: ManagedExecutionOwner,
  ): Promise<ExecutionRecord | null> {
    return this.queryOne(
      `SELECT ${SELECT_COLUMNS} FROM execution
       WHERE idempotency_key = ? AND owner_domain = ? AND owner_digest = ? LIMIT 1`,
      [idempotency_key, owner.domain, owner.digest],
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
    owner: input.owner,
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
    owner: { domain: row.owner_domain, digest: row.owner_digest },
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
