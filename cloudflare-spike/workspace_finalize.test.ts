import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { DatabaseSync, type StatementSync } from "node:sqlite";
import test from "node:test";

import type {
  RelationalDatabase,
  RelationalResult,
  RelationalStatement,
} from "./src/execution_store.ts";
import {
  D1WorkspaceFinalizeStore,
  workspaceFinalizeIdentity,
} from "./src/workspace_finalize.ts";

const migration = readFileSync(
  new URL("./migrations/0003_workspace_finalize.sql", import.meta.url),
  "utf8",
);

test("canonical session/request identity is stable across object key order", async () => {
  const first = await workspaceFinalizeIdentity("session-1", {
    sessionId: "session-1",
    workspace: { snapshot: "a".repeat(64), password: "secret" },
  });
  const reordered = await workspaceFinalizeIdentity("session-1", {
    workspace: { password: "secret", snapshot: "a".repeat(64) },
    sessionId: "session-1",
  });
  assert.deepEqual(first, reordered);
  assert.notDeepEqual(
    first,
    await workspaceFinalizeIdentity("session-1", {
      sessionId: "session-1",
      workspace: { snapshot: "b".repeat(64), password: "secret" },
    }),
  );
});

test("durable finalize claims replay exact requests and conflict on changed session content", async () => {
  const sqlite = new DatabaseSync(":memory:");
  sqlite.exec(migration);
  const store = new D1WorkspaceFinalizeStore(new SqliteDatabase(sqlite));
  const identity = await workspaceFinalizeIdentity("session-1", {
    sessionId: "session-1",
  });
  const input = { ...identity, session_id: "session-1", now_ms: 10 };

  assert.equal((await store.claim(input)).kind, "created");
  assert.equal((await store.claim({ ...input, now_ms: 11 })).kind, "reused");

  const changed = await workspaceFinalizeIdentity("session-1", {
    sessionId: "session-1",
    workspace: { snapshot: "b".repeat(64) },
  });
  assert.equal(
    (await store.claim({ ...changed, session_id: "session-1", now_ms: 12 }))
      .kind,
    "conflict",
  );

  assert.equal(
    await store.complete({
      finalize_id: identity.finalize_id,
      result_snapshot: "c".repeat(64),
      now_ms: 20,
    }),
    true,
  );
  assert.equal(
    await store.complete({
      finalize_id: identity.finalize_id,
      result_snapshot: "d".repeat(64),
      now_ms: 21,
    }),
    false,
  );
  const replay = await store.claim({ ...input, now_ms: 30 });
  assert.equal(replay.kind, "reused");
  assert.equal(replay.record.status, "completed");
  assert.equal(replay.record.result_snapshot, "c".repeat(64));
});

test("a surviving running claim is replayed without becoming a new owner", async () => {
  const sqlite = new DatabaseSync(":memory:");
  sqlite.exec(migration);
  const store = new D1WorkspaceFinalizeStore(new SqliteDatabase(sqlite));
  const identity = await workspaceFinalizeIdentity("session-crash", {
    sessionId: "session-crash",
  });
  const input = { ...identity, session_id: "session-crash", now_ms: 10 };
  assert.equal((await store.claim(input)).kind, "created");

  const recovery = await store.claim({ ...input, now_ms: 60_000 });
  assert.equal(recovery.kind, "reused");
  assert.equal(recovery.record.status, "running");
});

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
    return { results: this.statement.all(...this.values) as T[] };
  }

  async run(): Promise<RelationalResult> {
    const result = this.statement.run(...this.values);
    return { meta: { changes: Number(result.changes) } };
  }
}
