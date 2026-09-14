import type { ManagedExecutionOwner } from "./managed_ownership.js";

interface WorkspaceFinalizeResult<T = unknown> {
  readonly results?: readonly T[];
  readonly meta?: { readonly changes?: number };
}

interface WorkspaceFinalizeStatement {
  bind(...values: readonly unknown[]): WorkspaceFinalizeStatement;
  all<T>(): Promise<WorkspaceFinalizeResult<T>>;
  run(): Promise<WorkspaceFinalizeResult>;
}

interface WorkspaceFinalizeDatabase {
  prepare(sql: string): WorkspaceFinalizeStatement;
}

export interface WorkspaceFinalizeRecord {
  readonly finalize_id: `sha256:${string}`;
  readonly session_id: string;
  readonly request_digest: `sha256:${string}`;
  readonly target_owner: ManagedExecutionOwner;
  readonly status: "running" | "completed" | "failed";
  readonly result_snapshot: string | null;
  readonly error_code: string | null;
  readonly error_message: string | null;
  readonly created_at_ms: number;
  readonly updated_at_ms: number;
}

export type WorkspaceFinalizeClaim =
  | { readonly kind: "created"; readonly record: WorkspaceFinalizeRecord }
  | { readonly kind: "reused"; readonly record: WorkspaceFinalizeRecord }
  | { readonly kind: "conflict"; readonly record: WorkspaceFinalizeRecord };

export interface WorkspaceFinalizeStore {
  claim(input: {
    readonly finalize_id: `sha256:${string}`;
    readonly session_id: string;
    readonly request_digest: `sha256:${string}`;
    readonly target_owner: ManagedExecutionOwner;
    readonly now_ms: number;
  }): Promise<WorkspaceFinalizeClaim>;
  get(
    sessionId: string,
    targetOwner: ManagedExecutionOwner,
  ): Promise<WorkspaceFinalizeRecord | null>;
  complete(input: {
    readonly finalize_id: `sha256:${string}`;
    readonly target_owner: ManagedExecutionOwner;
    readonly result_snapshot: string;
    readonly now_ms: number;
  }): Promise<boolean>;
  fail(input: {
    readonly finalize_id: `sha256:${string}`;
    readonly target_owner: ManagedExecutionOwner;
    readonly error_code: string;
    readonly error_message: string;
    readonly now_ms: number;
  }): Promise<boolean>;
}

export class D1WorkspaceFinalizeStore implements WorkspaceFinalizeStore {
  private readonly database: WorkspaceFinalizeDatabase;

  constructor(database: WorkspaceFinalizeDatabase) {
    this.database = database;
  }

  async claim(input: {
    readonly finalize_id: `sha256:${string}`;
    readonly session_id: string;
    readonly request_digest: `sha256:${string}`;
    readonly target_owner: ManagedExecutionOwner;
    readonly now_ms: number;
  }): Promise<WorkspaceFinalizeClaim> {
    const inserted = await this.database
      .prepare(
        `INSERT OR IGNORE INTO workspace_finalize (
         finalize_id, session_id, request_digest, status, created_at_ms,
         updated_at_ms, target_owner_domain, target_owner_digest
       ) VALUES (?, ?, ?, 'running', ?, ?, ?, ?)`,
      )
      .bind(
        input.finalize_id,
        input.session_id,
        input.request_digest,
        input.now_ms,
        input.now_ms,
        input.target_owner.domain,
        input.target_owner.digest,
      )
      .run();
    const record = await this.get(input.session_id, input.target_owner);
    if (record === null)
      throw new Error("workspace finalize claim was not readable");
    if (
      record.finalize_id !== input.finalize_id ||
      record.request_digest !== input.request_digest
    ) {
      return { kind: "conflict", record };
    }
    return {
      kind: (inserted.meta?.changes ?? 0) > 0 ? "created" : "reused",
      record,
    };
  }

  async get(
    sessionId: string,
    targetOwner: ManagedExecutionOwner,
  ): Promise<WorkspaceFinalizeRecord | null> {
    const result = await this.database
      .prepare(
        `SELECT finalize_id, session_id, request_digest, status, result_snapshot,
              target_owner_domain, target_owner_digest,
              error_code, error_message, created_at_ms, updated_at_ms
       FROM workspace_finalize WHERE session_id = ?
         AND target_owner_domain = ? AND target_owner_digest = ? LIMIT 1`,
      )
      .bind(sessionId, targetOwner.domain, targetOwner.digest)
      .all<WorkspaceFinalizeRow>();
    const rows = result.results ?? [];
    if (rows.length > 1)
      throw new Error(
        "indexed workspace finalize query returned multiple rows",
      );
    const row = rows[0];
    return row === undefined ? null : recordFromRow(row);
  }

  async complete(input: {
    readonly finalize_id: `sha256:${string}`;
    readonly target_owner: ManagedExecutionOwner;
    readonly result_snapshot: string;
    readonly now_ms: number;
  }): Promise<boolean> {
    const result = await this.database
      .prepare(
        `UPDATE workspace_finalize SET status = 'completed', result_snapshot = ?, updated_at_ms = ?
       WHERE finalize_id = ? AND target_owner_domain = ?
         AND target_owner_digest = ? AND status = 'running'`,
      )
      .bind(
        input.result_snapshot,
        input.now_ms,
        input.finalize_id,
        input.target_owner.domain,
        input.target_owner.digest,
      )
      .run();
    return (result.meta?.changes ?? 0) === 1;
  }

  async fail(input: {
    readonly finalize_id: `sha256:${string}`;
    readonly target_owner: ManagedExecutionOwner;
    readonly error_code: string;
    readonly error_message: string;
    readonly now_ms: number;
  }): Promise<boolean> {
    const result = await this.database
      .prepare(
        `UPDATE workspace_finalize SET status = 'failed', error_code = ?, error_message = ?, updated_at_ms = ?
       WHERE finalize_id = ? AND target_owner_domain = ?
         AND target_owner_digest = ? AND status = 'running'`,
      )
      .bind(
        input.error_code,
        input.error_message,
        input.now_ms,
        input.finalize_id,
        input.target_owner.domain,
        input.target_owner.digest,
      )
      .run();
    return (result.meta?.changes ?? 0) === 1;
  }
}

interface WorkspaceFinalizeRow extends Omit<WorkspaceFinalizeRecord, "target_owner"> {
  readonly target_owner_domain: ManagedExecutionOwner["domain"];
  readonly target_owner_digest: `sha256:${string}`;
}

function recordFromRow(row: WorkspaceFinalizeRow): WorkspaceFinalizeRecord {
  return {
    finalize_id: row.finalize_id,
    session_id: row.session_id,
    request_digest: row.request_digest,
    target_owner: {
      domain: row.target_owner_domain,
      digest: row.target_owner_digest,
    },
    status: row.status,
    result_snapshot: row.result_snapshot,
    error_code: row.error_code,
    error_message: row.error_message,
    created_at_ms: row.created_at_ms,
    updated_at_ms: row.updated_at_ms,
  };
}

export async function workspaceFinalizeIdentity(
  sessionId: string,
  body: unknown,
): Promise<{
  readonly finalize_id: `sha256:${string}`;
  readonly request_digest: `sha256:${string}`;
}> {
  const request_digest = await sha256Digest(canonicalJson(body));
  const finalize_id = await sha256Digest(
    canonicalJson({ session_id: sessionId, request_digest }),
  );
  return { finalize_id, request_digest };
}

function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record)
    .sort((left, right) => (left < right ? -1 : left > right ? 1 : 0))
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(record[key])}`)
    .join(",")}}`;
}

async function sha256Digest(value: string): Promise<`sha256:${string}`> {
  const bytes = new Uint8Array(
    await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)),
  );
  return `sha256:${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}
