import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { DatabaseSync, type StatementSync } from "node:sqlite";
import { test } from "node:test";
import {
  D1ExecutionStore,
  ManagedExecutionAllowanceError,
  parseManagedExecutionAllowance,
  type ExecutionClaimInput,
  type ManagedExecutionAllowance,
  type RelationalDatabase,
  type RelationalResult,
  type RelationalStatement,
} from "./src/execution_store.ts";

const migration0001 = readFileSync(
  new URL("./migrations/0001_execution.sql", import.meta.url),
  "utf8",
);
const migration0002 = readFileSync(
  new URL("./migrations/0002_managed_execution_allowance.sql", import.meta.url),
  "utf8",
);
const hash = `sha256:${"a".repeat(64)}` as const;
const digest = `sha256:${"b".repeat(64)}` as const;
const allowance: ManagedExecutionAllowance = {
  deployment_epoch: "burnin-2026-09-01-a",
  execution_limit: 1,
};

test("allowance config accepts only a bounded explicit epoch and limit", () => {
  assert.deepEqual(
    parseManagedExecutionAllowance("burnin-2026-09-01-a", "3"),
    { deployment_epoch: "burnin-2026-09-01-a", execution_limit: 3 },
  );
  for (const [epoch, limit] of [
    [undefined, "3"],
    ["", "3"],
    [" leading-space", "3"],
    ["epoch", undefined],
    ["epoch", "0"],
    ["epoch", "01"],
    ["epoch", "1.5"],
    ["epoch", "1001"],
  ] as const) {
    assert.equal(parseManagedExecutionAllowance(epoch, limit), null);
  }
});

test("only a genuinely new claim atomically reserves the singleton allowance", async () => {
  const { database, sqlite } = seededDatabase(allowance);
  const store = new D1ExecutionStore(database);

  assert.equal((await store.claim(claim(), allowance)).kind, "created");
  assert.equal(reserved(sqlite), 1);

  assert.equal(
    (await store.claim(claim({ owner_token: "retry-owner" }), allowance)).kind,
    "reused",
  );
  assert.equal(reserved(sqlite), 1);

  assert.equal(
    (
      await store.claim(
        claim({ invocation_id: "invocation-2", owner_token: "owner-2" }),
        allowance,
      )
    ).kind,
    "conflict",
  );
  assert.equal(reserved(sqlite), 1);

  await store.get("invocation-1");
  assert.equal(reserved(sqlite), 1);
});

test("exhaustion and configuration mismatch create no invocation row", async () => {
  const { database, sqlite } = seededDatabase(allowance);
  const store = new D1ExecutionStore(database);
  await store.claim(claim(), allowance);

  await assert.rejects(
    store.claim(
      claim({
        invocation_id: "invocation-2",
        idempotency_key: "delivery-2",
        owner_token: "owner-2",
      }),
      allowance,
    ),
    ManagedExecutionAllowanceError,
  );
  await assert.rejects(
    store.claim(
      claim({
        invocation_id: "invocation-3",
        idempotency_key: "delivery-3",
        owner_token: "owner-3",
      }),
      { ...allowance, execution_limit: 2 },
    ),
    ManagedExecutionAllowanceError,
  );

  assert.equal(executionCount(sqlite), 1);
  assert.equal(reserved(sqlite), 1);
});

test("configuration alone never creates or resets allowance", async () => {
  const sqlite = new DatabaseSync(":memory:");
  applyAllMigrations(sqlite);
  const store = new D1ExecutionStore(new SqliteDatabase(sqlite));

  await assert.rejects(store.claim(claim(), allowance), ManagedExecutionAllowanceError);
  assert.equal(executionCount(sqlite), 0);
  assert.equal(allowanceCount(sqlite), 0);

  seedAllowance(sqlite, allowance);
  await store.claim(claim(), allowance);
  await assert.rejects(
    store.claim(
      claim({
        invocation_id: "invocation-2",
        idempotency_key: "delivery-2",
        owner_token: "owner-2",
      }),
      { deployment_epoch: "burnin-2026-09-01-b", execution_limit: 1 },
    ),
    ManagedExecutionAllowanceError,
  );
  assert.equal(reserved(sqlite), 1);

  sqlite.prepare(
    `UPDATE managed_execution_allowance
     SET deployment_epoch = ?, execution_limit = ?, reserved_executions = 0
     WHERE singleton = 1`,
  ).run("burnin-2026-09-01-b", 1);
  assert.equal(
    (
      await store.claim(
        claim({
          invocation_id: "invocation-2",
          idempotency_key: "delivery-2",
          owner_token: "owner-2",
        }),
        { deployment_epoch: "burnin-2026-09-01-b", execution_limit: 1 },
      )
    ).kind,
    "created",
  );
  assert.equal(reserved(sqlite), 1);
});

test("migration 0002 safely upgrades populated 0001 and fails old writers closed", async () => {
  const sqlite = new DatabaseSync(":memory:");
  sqlite.exec(migration0001);
  insertLegacyExecution(sqlite, "legacy-invocation", "legacy-delivery");

  assert.doesNotThrow(() => sqlite.exec(migration0002));
  assert.deepEqual(
    {
      ...(sqlite.prepare(
        "SELECT allowance_epoch, allowance_limit FROM execution WHERE invocation_id = ?",
      ).get("legacy-invocation") as {
        allowance_epoch: string;
        allowance_limit: number;
      }),
    },
    { allowance_epoch: "__pre_allowance__", allowance_limit: 1 },
  );

  assert.throws(
    () => insertLegacyExecution(sqlite, "old-writer-invocation", "old-writer-delivery"),
    /managed_execution_allowance_unavailable/,
  );
  assert.equal(executionCount(sqlite), 1);

  seedAllowance(sqlite, allowance);
  const store = new D1ExecutionStore(new SqliteDatabase(sqlite));
  assert.equal(
    (
      await store.claim(
        claim({
          invocation_id: "new-writer-invocation",
          idempotency_key: "new-writer-delivery",
        }),
        allowance,
      )
    ).kind,
    "created",
  );
  assert.equal(executionCount(sqlite), 2);
  assert.equal(reserved(sqlite), 1);
});

function claim(changes: Partial<ExecutionClaimInput> = {}): ExecutionClaimInput {
  return {
    invocation_id: "invocation-1",
    idempotency_key: "delivery-1",
    request_hash: hash,
    execution_digest: digest,
    execution_policy_revision: "managed/1",
    session_id: "session-1",
    attribution: {
      harness: "opencode",
      transport: "http",
      requested_model: "zai-coding-plan/glm-4.5-air",
      served_model: null,
    },
    owner_token: "owner-1",
    now_ms: 1_000,
    lease_expires_at_ms: 601_000,
    ...changes,
  };
}

function seededDatabase(config: ManagedExecutionAllowance): {
  readonly database: RelationalDatabase;
  readonly sqlite: DatabaseSync;
} {
  const sqlite = new DatabaseSync(":memory:");
  applyAllMigrations(sqlite);
  seedAllowance(sqlite, config);
  return { database: new SqliteDatabase(sqlite), sqlite };
}

function applyAllMigrations(sqlite: DatabaseSync): void {
  sqlite.exec(migration0001);
  sqlite.exec(migration0002);
}

function insertLegacyExecution(
  sqlite: DatabaseSync,
  invocationId: string,
  idempotencyKey: string,
): void {
  sqlite.prepare(
    `INSERT INTO execution (
      invocation_id, idempotency_key, request_hash, execution_digest,
      execution_policy_revision, session_id, harness, transport,
      requested_model, status, owner_token, lease_expires_at_ms,
      created_at_ms, updated_at_ms
    ) VALUES (?, ?, ?, ?, 'managed/legacy', 'legacy-session', 'opencode',
      'http', 'legacy/model', 'running', 'legacy-owner', 601000, 1000, 1000)`,
  ).run(invocationId, idempotencyKey, hash, digest);
}

function seedAllowance(
  sqlite: DatabaseSync,
  config: ManagedExecutionAllowance,
): void {
  sqlite.prepare(
    `INSERT INTO managed_execution_allowance (
      singleton, deployment_epoch, execution_limit, reserved_executions
    ) VALUES (1, ?, ?, 0)`,
  ).run(config.deployment_epoch, config.execution_limit);
}

function reserved(sqlite: DatabaseSync): number {
  return Number(
    (
      sqlite.prepare(
        "SELECT reserved_executions FROM managed_execution_allowance WHERE singleton = 1",
      ).get() as { reserved_executions: number }
    ).reserved_executions,
  );
}

function executionCount(sqlite: DatabaseSync): number {
  return Number(
    (sqlite.prepare("SELECT count(*) AS count FROM execution").get() as { count: number })
      .count,
  );
}

function allowanceCount(sqlite: DatabaseSync): number {
  return Number(
    (
      sqlite.prepare("SELECT count(*) AS count FROM managed_execution_allowance").get() as {
        count: number;
      }
    ).count,
  );
}

class SqliteDatabase implements RelationalDatabase {
  private readonly sqlite: DatabaseSync;

  constructor(sqlite: DatabaseSync) {
    this.sqlite = sqlite;
  }

  prepare(sql: string): RelationalStatement {
    return new SqliteStatement(this.sqlite.prepare(sql));
  }
}

class SqliteStatement implements RelationalStatement {
  private readonly statement: StatementSync;
  private values: readonly unknown[] = [];

  constructor(statement: StatementSync) {
    this.statement = statement;
  }

  bind(...values: readonly unknown[]): RelationalStatement {
    this.values = values;
    return this;
  }

  async all<T>(): Promise<RelationalResult<T>> {
    const results = this.statement.all(...this.values) as T[];
    return { results, meta: { rows_read: results.length, rows_written: 0 } };
  }

  async run(): Promise<RelationalResult> {
    const result = this.statement.run(...this.values);
    return {
      meta: {
        changes: Number(result.changes),
        rows_read: 0,
        rows_written: Number(result.changes),
      },
    };
  }
}
