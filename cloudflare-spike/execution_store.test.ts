import assert from "node:assert/strict";
import { test } from "node:test";
import type { ExecutionArtifactRef } from "./src/codex_execution.ts";
import {
  D1ExecutionStore,
  type ExecutionClaimInput,
  type RelationalDatabase,
  type RelationalResult,
  type RelationalStatement,
  type RelationalUsage,
} from "./src/execution_store.ts";

const hashA = `sha256:${"a".repeat(64)}` as const;
const hashB = `sha256:${"b".repeat(64)}` as const;
const digest = `sha256:${"c".repeat(64)}` as const;
const artifact: ExecutionArtifactRef = {
  key: "executions/a/b.json",
  media_type: "application/json",
  bytes: 42,
  sha256: hashB,
};

function claim(
  changes: Partial<ExecutionClaimInput> = {},
): ExecutionClaimInput {
  return {
    invocation_id: "invocation-1",
    idempotency_key: "delivery-1",
    request_hash: hashA,
    execution_digest: digest,
    execution_policy_revision: "policy/1",
    session_id: "session-1",
    owner_token: "owner-1",
    now_ms: 1_000,
    lease_expires_at_ms: 601_000,
    ...changes,
  };
}

test("happy path uses one claim write and one terminal write", async () => {
  const database = new FakeDatabase();
  const usage: RelationalUsage[] = [];
  const store = new D1ExecutionStore(database, (item) => usage.push(item));

  const created = await store.claim(claim());
  assert.equal(created.kind, "created");
  assert.equal(
    await store.finish({
      invocation_id: "invocation-1",
      request_hash: hashA,
      owner_token: "owner-1",
      status: "completed",
      artifact_ref: artifact,
      now_ms: 2_000,
    }),
    true,
  );

  assert.equal(sum(usage, "rows_written"), 2);
  assert.ok(sum(usage, "rows_read") <= 1);
  assert.equal((await store.get("invocation-1"))?.artifact_ref?.key, artifact.key);
});

test("exact retries reuse the row without another write", async () => {
  const database = new FakeDatabase();
  const usage: RelationalUsage[] = [];
  const store = new D1ExecutionStore(database, (item) => usage.push(item));
  await store.claim(claim());
  usage.length = 0;

  const retry = await store.claim(claim({ owner_token: "ignored-new-owner" }));
  assert.equal(retry.kind, "reused");
  assert.equal(retry.record.owner_token, "owner-1");
  assert.equal(sum(usage, "rows_written"), 0);
  assert.equal(sum(usage, "rows_read"), 1);
});

test("changed content or reused idempotency keys conflict", async () => {
  const database = new FakeDatabase();
  const store = new D1ExecutionStore(database);
  await store.claim(claim());

  assert.equal(
    (await store.claim(claim({ request_hash: hashB }))).kind,
    "conflict",
  );
  assert.equal(
    (
      await store.claim(
        claim({ invocation_id: "invocation-2", owner_token: "owner-2" }),
      )
    ).kind,
    "conflict",
  );
  assert.equal(database.rows.size, 1);
});

test("only the live owner can terminalize a running execution", async () => {
  const store = new D1ExecutionStore(new FakeDatabase());
  await store.claim(claim());
  assert.equal(
    await store.finish({
      invocation_id: "invocation-1",
      request_hash: hashA,
      owner_token: "other-owner",
      status: "failed",
      artifact_ref: artifact,
      now_ms: 2_000,
    }),
    false,
  );
  assert.equal((await store.get("invocation-1"))?.status, "running");
});

function sum(items: readonly RelationalUsage[], key: keyof RelationalUsage): number {
  return items.reduce((total, item) => total + item[key], 0);
}

class FakeDatabase implements RelationalDatabase {
  readonly rows = new Map<string, Record<string, unknown>>();

  prepare(sql: string): RelationalStatement {
    return new FakeStatement(this, sql);
  }
}

class FakeStatement implements RelationalStatement {
  private values: readonly unknown[] = [];
  private readonly database: FakeDatabase;
  private readonly sql: string;

  constructor(database: FakeDatabase, sql: string) {
    this.database = database;
    this.sql = sql;
  }

  bind(...values: readonly unknown[]): RelationalStatement {
    this.values = values;
    return this;
  }

  async all<T>(): Promise<RelationalResult<T>> {
    let row: Record<string, unknown> | undefined;
    if (this.sql.includes("WHERE invocation_id = ?")) {
      row = this.database.rows.get(String(this.values[0]));
    } else if (this.sql.includes("WHERE idempotency_key = ?")) {
      row = [...this.database.rows.values()].find(
        (item) => item.idempotency_key === this.values[0],
      );
    } else {
      throw new Error(`unexpected query: ${this.sql}`);
    }
    return {
      results: row === undefined ? [] : [row as T],
      meta: { rows_read: row === undefined ? 0 : 1, rows_written: 0 },
    };
  }

  async run(): Promise<RelationalResult> {
    if (this.sql.startsWith("INSERT OR IGNORE")) {
      const [
        invocation_id,
        idempotency_key,
        request_hash,
        execution_digest,
        execution_policy_revision,
        session_id,
        owner_token,
        lease_expires_at_ms,
        created_at_ms,
        updated_at_ms,
      ] = this.values;
      const duplicate =
        this.database.rows.has(String(invocation_id)) ||
        [...this.database.rows.values()].some(
          (row) => row.idempotency_key === idempotency_key,
        );
      if (duplicate) return { meta: { changes: 0, rows_written: 0 } };
      this.database.rows.set(String(invocation_id), {
        invocation_id,
        idempotency_key,
        request_hash,
        execution_digest,
        execution_policy_revision,
        session_id,
        status: "running",
        owner_token,
        lease_expires_at_ms,
        created_at_ms,
        updated_at_ms,
        artifact_key: null,
        artifact_media_type: null,
        artifact_bytes: null,
        artifact_sha256: null,
      });
      return { meta: { changes: 1, rows_written: 1 } };
    }
    if (this.sql.startsWith("UPDATE execution")) {
      const [
        status,
        artifact_key,
        artifact_media_type,
        artifact_bytes,
        artifact_sha256,
        updated_at_ms,
        invocation_id,
        request_hash,
        owner_token,
      ] = this.values;
      const row = this.database.rows.get(String(invocation_id));
      if (
        row === undefined ||
        row.request_hash !== request_hash ||
        row.owner_token !== owner_token ||
        row.status !== "running"
      ) {
        return { meta: { changes: 0, rows_written: 0 } };
      }
      Object.assign(row, {
        status,
        artifact_key,
        artifact_media_type,
        artifact_bytes,
        artifact_sha256,
        updated_at_ms,
      });
      return { meta: { changes: 1, rows_written: 1 } };
    }
    throw new Error(`unexpected mutation: ${this.sql}`);
  }
}
