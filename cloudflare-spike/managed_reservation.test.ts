import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { DatabaseSync, type StatementSync } from "node:sqlite";
import { test } from "node:test";
import type {
  RelationalDatabase,
  RelationalResult,
  RelationalStatement,
} from "./src/execution_store.ts";
import type { ManagedExecutionOwner } from "./src/managed_ownership.ts";
import {
  D1ManagedExecutionReservationStore,
  ManagedExecutionAllowanceError,
  ManagedReservationAccessError,
  type ManagedExecutionAllowance,
} from "./src/managed_reservation.ts";

const migrations = [1, 2, 3, 4].map((number) =>
  readFileSync(
    new URL(
      `./migrations/000${number}_${[
        "execution",
        "managed_execution_allowance",
        "workspace_finalize",
        "managed_reservation_ownership",
      ][number - 1]}.sql`,
      import.meta.url,
    ),
    "utf8",
  ),
);
const allowance: ManagedExecutionAllowance = {
  deployment_epoch: "burnin-2026-09-08-a",
  execution_limit: 2,
};
const ownerA: ManagedExecutionOwner = {
  domain: "huddles_workspace",
  digest: `sha256:${"a".repeat(64)}`,
};
const ownerB: ManagedExecutionOwner = {
  domain: "huddles_workspace",
  digest: `sha256:${"b".repeat(64)}`,
};
const requestHash = `sha256:${"c".repeat(64)}` as const;
const provisionDigest = `sha256:${"d".repeat(64)}` as const;

test("provision reserves once, exact retries do no work, and execution consumes readiness", async () => {
  const { sqlite, store } = seeded();
  const input = provision();
  assert.equal((await store.claimProvision(input, allowance)).kind, "created");
  assert.equal(reserved(sqlite), 1);

  const retry = await store.claimProvision({ ...input, now_ms: 2 }, allowance);
  assert.equal(retry.kind, "reused");
  assert.equal(retry.record.status, "provisioning");
  assert.equal(reserved(sqlite), 1);

  assert.equal(
    await store.markReady({ invocation_id: input.invocation_id, owner: ownerA, now_ms: 3 }),
    true,
  );
  const execution = await store.claimExecution(
    {
      invocation_id: input.invocation_id,
      session_id: input.session_id,
      owner: ownerA,
      execution_request_hash: requestHash,
      now_ms: 4,
    },
    allowance,
  );
  assert.equal(execution.kind, "reused");
  assert.equal(execution.record.source, "workspace_provision");
  assert.equal(execution.record.status, "ready");
  assert.equal(reserved(sqlite), 1);

  const later = await store.claimExecution(
    direct("invocation-b", input.session_id, ownerA),
    allowance,
  );
  assert.equal(later.kind, "created");
  assert.equal(reserved(sqlite), 2);
});

test("changed provision identity conflicts and ambiguous failure stays consumed", async () => {
  const { sqlite, store } = seeded();
  const input = provision();
  await store.claimProvision(input, allowance);
  assert.equal(
    (await store.claimProvision({ ...input, execution_request_hash: `sha256:${"e".repeat(64)}` }, allowance)).kind,
    "conflict",
  );
  assert.equal(
    await store.markFailed({
      invocation_id: input.invocation_id,
      owner: ownerA,
      error_code: "restore_failed",
      now_ms: 5,
    }),
    true,
  );
  const retry = await store.claimProvision({ ...input, now_ms: 6 }, allowance);
  assert.equal(retry.kind, "reused");
  assert.equal(retry.record.status, "failed");
  assert.equal(retry.record.error_code, "restore_failed");
  assert.equal(reserved(sqlite), 1);
  assert.equal(
    await store.getReady(input.invocation_id, input.session_id, ownerA, requestHash, allowance),
    null,
  );
});

test("concurrent exact claims have one winner and reserve one shared slot", async () => {
  const { sqlite, store } = seeded();
  const input = provision();
  const claims = await Promise.all(
    Array.from({ length: 8 }, (_, index) =>
      store.claimProvision({ ...input, now_ms: 10 + index }, allowance),
    ),
  );
  assert.equal(claims.filter(({ kind }) => kind === "created").length, 1);
  assert.ok(claims.every(({ kind }) => kind === "created" || kind === "reused"));
  assert.equal(reserved(sqlite), 1);
  assert.equal(count(sqlite, "managed_execution_reservation"), 1);
});

test("session ownership is immutable and foreign workspace access is opaque", async () => {
  const { sqlite, store } = seeded();
  await store.claimProvision(provision(), allowance);
  await assert.rejects(
    store.claimExecution(
      {
        invocation_id: "invocation-b",
        session_id: "session-a",
        owner: ownerB,
        execution_request_hash: requestHash,
        now_ms: 2,
      },
      allowance,
    ),
    ManagedReservationAccessError,
  );
  assert.equal(reserved(sqlite), 1);
  assert.deepEqual(await store.getSessionOwner("session-a"), ownerA);
  assert.throws(
    () => sqlite.prepare("UPDATE managed_session_owner SET owner_digest = ? WHERE session_id = ?").run(ownerB.digest, "session-a"),
    /managed_session_owner_immutable/,
  );
});

test("capacity/config mismatch rolls back reservation and session owner atomically", async () => {
  const { sqlite, store } = seeded({ ...allowance, execution_limit: 1 });
  await store.claimExecution(direct("invocation-a", "session-a", ownerA), {
    ...allowance,
    execution_limit: 1,
  });
  await assert.rejects(
    store.claimExecution(direct("invocation-b", "session-b", ownerB), {
      ...allowance,
      execution_limit: 1,
    }),
    ManagedExecutionAllowanceError,
  );
  await assert.rejects(
    store.claimExecution(direct("invocation-c", "session-c", ownerB), allowance),
    ManagedExecutionAllowanceError,
  );
  assert.equal(count(sqlite, "managed_execution_reservation"), 1);
  assert.equal(count(sqlite, "managed_session_owner"), 1);
  assert.equal(reserved(sqlite), 1);
});

test("0004 preserves legacy counters but makes unowned rows inaccessible and rejects old writers", async () => {
  const sqlite = new DatabaseSync(":memory:");
  sqlite.exec(migrations[0]);
  sqlite.exec(migrations[1]);
  sqlite.exec(migrations[2]);
  sqlite.prepare(
    `INSERT INTO managed_execution_allowance (
      singleton, deployment_epoch, execution_limit, reserved_executions
    ) VALUES (1, ?, ?, 0)`,
  ).run(allowance.deployment_epoch, allowance.execution_limit);
  sqlite.prepare(
    `INSERT INTO execution (
      invocation_id, idempotency_key, request_hash, execution_digest,
      execution_policy_revision, session_id, harness, transport, requested_model,
      status, owner_token, lease_expires_at_ms, created_at_ms, updated_at_ms,
      allowance_epoch, allowance_limit
    ) VALUES ('legacy-invocation', 'legacy-invocation', ?, ?, 'managed/1',
      'legacy-session', 'opencode', 'http', 'provider/model', 'running', 'token',
      2, 1, 1, ?, ?)`,
  ).run(requestHash, provisionDigest, allowance.deployment_epoch, allowance.execution_limit);
  sqlite.prepare(
    `INSERT INTO workspace_finalize (
      finalize_id, session_id, request_digest, status, created_at_ms, updated_at_ms
    ) VALUES ('legacy-finalize', 'legacy-session', ?, 'running', 1, 1)`,
  ).run(provisionDigest);
  sqlite.exec(migrations[3]);
  assert.equal(reserved(sqlite), 1);
  const legacy = sqlite.prepare(
    "SELECT owner_domain, owner_digest FROM execution WHERE invocation_id = 'legacy-invocation'",
  ).get() as { owner_domain: string; owner_digest: string };
  assert.deepEqual({ ...legacy }, { owner_domain: "legacy_unowned", owner_digest: "legacy_unowned" });
  const store = new D1ManagedExecutionReservationStore(new SqliteDatabase(sqlite));
  assert.equal(await store.getSessionOwner("legacy-session"), null);
  assert.throws(
    () => sqlite.prepare(
      `INSERT INTO execution (
        invocation_id, idempotency_key, request_hash, execution_digest,
        execution_policy_revision, session_id, harness, transport, requested_model,
        status, owner_token, lease_expires_at_ms, created_at_ms, updated_at_ms,
        allowance_epoch, allowance_limit, owner_domain, owner_digest
      ) VALUES ('old-writer', 'old-writer', ?, ?, 'managed/1', 'session', 'opencode',
        'http', 'provider/model', 'running', 'token', 2, 1, 1, ?, ?, ?, ?)`,
    ).run(requestHash, provisionDigest, allowance.deployment_epoch, allowance.execution_limit, ownerA.domain, ownerA.digest),
    /managed_execution_reservation_unavailable/,
  );
  assert.throws(
    () => sqlite.prepare("UPDATE execution SET owner_domain = ?, owner_digest = ? WHERE invocation_id = 'legacy-invocation'").run(ownerA.domain, ownerA.digest),
    /managed_execution_owner_immutable/,
  );
  assert.throws(
    () => sqlite.prepare("UPDATE workspace_finalize SET target_owner_domain = ?, target_owner_digest = ? WHERE finalize_id = 'legacy-finalize'").run(ownerA.domain, ownerA.digest),
    /workspace_finalize_owner_immutable/,
  );
});

test("SQL rejects malformed owner digests and immutable reservation identity updates", async () => {
  const { sqlite, store } = seeded();
  await store.claimExecution(direct("invocation-a", "session-a", ownerA), allowance);
  assert.throws(
    () => sqlite.prepare(
      `INSERT INTO managed_session_owner (session_id, owner_domain, owner_digest, created_at_ms)
       VALUES ('bad', 'huddles_workspace', ?, 1)`,
    ).run(`sha256:${"a".repeat(63)}z`),
    /CHECK constraint failed/,
  );
  for (const [column, value] of [
    ["session_id", "changed-session"],
    ["execution_request_hash", `sha256:${"e".repeat(64)}`],
    ["allowance_epoch", "changed-epoch"],
    ["allowance_limit", 1],
    ["created_at_ms", 2],
    ["source", "workspace_provision"],
    ["provision_request_digest", provisionDigest],
  ] as const) {
    assert.throws(
      () => sqlite.prepare(`UPDATE managed_execution_reservation SET ${column} = ? WHERE invocation_id = 'invocation-a'`).run(value),
      /managed_execution_reservation_identity_immutable/,
    );
  }
});

function provision() {
  return {
    invocation_id: "invocation-a",
    session_id: "session-a",
    owner: ownerA,
    execution_request_hash: requestHash,
    source: "workspace_provision" as const,
    provision_request_digest: provisionDigest,
    now_ms: 1,
  };
}

function direct(invocation_id: string, session_id: string, owner: ManagedExecutionOwner) {
  return { invocation_id, session_id, owner, execution_request_hash: requestHash, now_ms: 1 };
}

function seeded(config: ManagedExecutionAllowance = allowance) {
  const sqlite = new DatabaseSync(":memory:");
  for (const migration of migrations) sqlite.exec(migration);
  sqlite.prepare(
    `INSERT INTO managed_execution_allowance (
      singleton, deployment_epoch, execution_limit, reserved_executions
    ) VALUES (1, ?, ?, 0)`,
  ).run(config.deployment_epoch, config.execution_limit);
  return {
    sqlite,
    store: new D1ManagedExecutionReservationStore(new SqliteDatabase(sqlite)),
  };
}

function reserved(sqlite: DatabaseSync): number {
  return Number((sqlite.prepare(
    "SELECT reserved_executions FROM managed_execution_allowance WHERE singleton = 1",
  ).get() as { reserved_executions: number }).reserved_executions);
}

function count(sqlite: DatabaseSync, table: string): number {
  return Number((sqlite.prepare(`SELECT count(*) AS value FROM ${table}`).get() as { value: number }).value);
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
  private values: readonly unknown[] = [];
  private readonly statement: StatementSync;
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
    return { meta: { changes: Number(result.changes), rows_read: 0, rows_written: Number(result.changes) } };
  }
}
