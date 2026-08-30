import assert from "node:assert/strict";
import { test } from "node:test";
import {
  MAX_EXECUTION_EVIDENCE_EVENTS,
  R2ExecutionArtifactStore,
  type ExecutionArtifact,
  type ObjectUsage,
} from "./src/execution_artifacts.ts";

const requestHash = `sha256:${"a".repeat(64)}` as const;

function artifact(
  changes: Partial<ExecutionArtifact> = {},
): ExecutionArtifact {
  return {
    version: 1,
    invocation_id: "invocation-1",
    request_hash: requestHash,
    terminal_result: { status: "completed", output: { text: "done" } },
    evidence: [{ type: "message_end", messageId: "message-1" }],
    ...changes,
  };
}

test("writes and verifies one immutable execution artifact", async () => {
  const bucket = new FakeBucket();
  const usage: ObjectUsage[] = [];
  const store = new R2ExecutionArtifactStore(
    bucket as unknown as Pick<R2Bucket, "get" | "put">,
    (item) => usage.push(item),
  );
  const value = artifact();

  const ref = await store.write(value);
  assert.match(ref.key, /^executions\/[0-9a-f]{64}\/[0-9a-f]{64}\.json$/);
  assert.deepEqual(await store.read(ref), value);
  assert.equal(sum(usage, "writes"), 1);
  assert.equal(sum(usage, "reads"), 1);
  assert.equal(sum(usage, "bytes_written"), ref.bytes);
  assert.equal(sum(usage, "bytes_read"), ref.bytes);
});

test("an exact retry verifies the winner without overwriting", async () => {
  const bucket = new FakeBucket();
  const usage: ObjectUsage[] = [];
  const store = new R2ExecutionArtifactStore(
    bucket as unknown as Pick<R2Bucket, "get" | "put">,
    (item) => usage.push(item),
  );
  const value = artifact();
  const first = await store.write(value);
  const second = await store.write(value);
  assert.deepEqual(second, first);
  assert.equal(sum(usage, "writes"), 2);
  assert.equal(sum(usage, "reads"), 1);
  assert.equal(bucket.objects.size, 1);
});

test("changed bytes at a deterministic key fail closed", async () => {
  const bucket = new FakeBucket();
  const store = new R2ExecutionArtifactStore(
    bucket as unknown as Pick<R2Bucket, "get" | "put">,
  );
  await store.write(artifact());
  await assert.rejects(
    store.write(artifact({ terminal_result: { status: "failed" } })),
    /immutable execution artifact conflict/,
  );
});

test("evidence cardinality is bounded before touching R2", async () => {
  const bucket = new FakeBucket();
  const store = new R2ExecutionArtifactStore(
    bucket as unknown as Pick<R2Bucket, "get" | "put">,
  );
  await assert.rejects(
    store.write(
      artifact({
        evidence: Array.from(
          { length: MAX_EXECUTION_EVIDENCE_EVENTS + 1 },
          () => ({ type: "message_delta" }),
        ),
      }),
    ),
    /maximum/,
  );
  assert.equal(bucket.puts, 0);
});

function sum(items: readonly ObjectUsage[], key: keyof ObjectUsage): number {
  return items.reduce((total, item) => total + item[key], 0);
}

class FakeBucket {
  readonly objects = new Map<string, Uint8Array>();
  puts = 0;

  async put(
    key: string,
    value: Uint8Array,
    options: { readonly onlyIf?: { readonly etagDoesNotMatch?: string } },
  ): Promise<{ readonly key: string } | null> {
    this.puts += 1;
    if (options.onlyIf?.etagDoesNotMatch === "*" && this.objects.has(key)) {
      return null;
    }
    this.objects.set(key, value.slice());
    return { key };
  }

  async get(
    key: string,
  ): Promise<{ readonly arrayBuffer: () => Promise<ArrayBuffer> } | null> {
    const value = this.objects.get(key);
    if (value === undefined) return null;
    return {
      arrayBuffer: async () => value.slice().buffer,
    };
  }
}
