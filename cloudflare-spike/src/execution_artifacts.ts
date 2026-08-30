import type {
  ExecutionArtifactRef,
  InvocationRequestHash,
  JsonValue,
  Sha256Digest,
} from "./codex_execution.js";

export const MAX_EXECUTION_EVIDENCE_EVENTS = 2_000;
export const MAX_EXECUTION_ARTIFACT_BYTES = 8 * 1024 * 1024;
export const MAX_EXECUTION_EVIDENCE_EVENT_BYTES = 256 * 1024;

export interface ExecutionArtifact {
  readonly version: 1;
  readonly invocation_id: string;
  readonly request_hash: InvocationRequestHash;
  readonly terminal_result: JsonValue;
  readonly evidence: readonly JsonValue[];
  readonly cost?: JsonValue;
}

export interface ObjectUsage {
  readonly reads: number;
  readonly writes: number;
  readonly bytes_read: number;
  readonly bytes_written: number;
}

export interface ExecutionArtifactStore {
  write(artifact: ExecutionArtifact): Promise<ExecutionArtifactRef>;
  read(ref: ExecutionArtifactRef): Promise<ExecutionArtifact>;
}

/** The deterministic key already contains another valid terminal winner. */
export class ExecutionArtifactConflictError extends Error {
  readonly existing_ref: ExecutionArtifactRef;
  readonly existing: ExecutionArtifact;

  constructor(existing_ref: ExecutionArtifactRef, existing: ExecutionArtifact) {
    super(`immutable execution artifact conflict at ${existing_ref.key}`);
    this.name = "ExecutionArtifactConflictError";
    this.existing_ref = existing_ref;
    this.existing = existing;
  }
}

type UsageObserver = (usage: ObjectUsage) => void;

export class R2ExecutionArtifactStore implements ExecutionArtifactStore {
  private readonly bucket: Pick<R2Bucket, "get" | "put">;
  private readonly observeUsage: UsageObserver;

  constructor(
    bucket: Pick<R2Bucket, "get" | "put">,
    observeUsage: UsageObserver = () => {},
  ) {
    this.bucket = bucket;
    this.observeUsage = observeUsage;
  }

  async write(artifact: ExecutionArtifact): Promise<ExecutionArtifactRef> {
    if (artifact.evidence.length > MAX_EXECUTION_EVIDENCE_EVENTS) {
      throw new Error(
        `execution evidence has ${artifact.evidence.length} events; maximum is ${MAX_EXECUTION_EVIDENCE_EVENTS}`,
      );
    }
    for (const [index, event] of artifact.evidence.entries()) {
      const bytes = new TextEncoder().encode(JSON.stringify(event)).byteLength;
      if (bytes > MAX_EXECUTION_EVIDENCE_EVENT_BYTES) {
        throw new Error(
          `execution evidence event ${index} is ${bytes} bytes; maximum is ${MAX_EXECUTION_EVIDENCE_EVENT_BYTES}`,
        );
      }
    }
    const body = new TextEncoder().encode(JSON.stringify(artifact));
    if (body.byteLength > MAX_EXECUTION_ARTIFACT_BYTES) {
      throw new Error(
        `execution artifact is ${body.byteLength} bytes; maximum is ${MAX_EXECUTION_ARTIFACT_BYTES}`,
      );
    }
    const sha256 = await digestBytes(body);
    const key = await artifactKey(artifact.invocation_id, artifact.request_hash);
    const ref: ExecutionArtifactRef = {
      key,
      media_type: "application/json",
      bytes: body.byteLength,
      sha256,
    };

    const stored = await this.bucket.put(key, body, {
      onlyIf: { etagDoesNotMatch: "*" },
      httpMetadata: { contentType: "application/json" },
      customMetadata: { sha256 },
    });
    this.observeUsage({
      reads: 0,
      writes: 1,
      bytes_read: 0,
      bytes_written: stored === null ? 0 : body.byteLength,
    });
    if (stored !== null) return ref;

    // An exact retry may race the first immutable put. Verify the winner rather
    // than overwriting it; a changed body at the deterministic key fails loud.
    let existing: { ref: ExecutionArtifactRef; artifact: ExecutionArtifact };
    try {
      existing = await this.readAtKey(key);
    } catch (error) {
      throw new Error(`immutable execution artifact conflict at ${key}`, {
        cause: error,
      });
    }
    if (
      existing.artifact.invocation_id !== artifact.invocation_id ||
      existing.artifact.request_hash !== artifact.request_hash
    ) {
      throw new Error(`immutable execution artifact identity mismatch at ${key}`);
    }
    if (JSON.stringify(existing.artifact) !== JSON.stringify(artifact)) {
      throw new ExecutionArtifactConflictError(
        existing.ref,
        existing.artifact,
      );
    }
    return existing.ref;
  }

  async read(ref: ExecutionArtifactRef): Promise<ExecutionArtifact> {
    const stored = await this.readAtKey(ref.key);
    if (stored.ref.bytes !== ref.bytes) {
      throw new Error(`execution artifact byte length mismatch at ${ref.key}`);
    }
    if (stored.ref.sha256 !== ref.sha256) {
      throw new Error(`execution artifact digest mismatch at ${ref.key}`);
    }
    return stored.artifact;
  }

  private async readAtKey(
    key: string,
  ): Promise<{ ref: ExecutionArtifactRef; artifact: ExecutionArtifact }> {
    const object = await this.bucket.get(key);
    if (object === null) throw new Error(`execution artifact missing at ${key}`);
    if (object.size > MAX_EXECUTION_ARTIFACT_BYTES) {
      throw new Error(
        `execution artifact is ${object.size} bytes; maximum is ${MAX_EXECUTION_ARTIFACT_BYTES}`,
      );
    }
    const body = new Uint8Array(await object.arrayBuffer());
    this.observeUsage({
      reads: 1,
      writes: 0,
      bytes_read: body.byteLength,
      bytes_written: 0,
    });
    const sha256 = await digestBytes(body);
    const value: unknown = JSON.parse(new TextDecoder().decode(body));
    if (!isExecutionArtifact(value)) {
      throw new Error(`execution artifact has invalid shape at ${key}`);
    }
    return {
      ref: {
        key,
        media_type: "application/json",
        bytes: body.byteLength,
        sha256,
      },
      artifact: value,
    };
  }
}

async function artifactKey(
  invocation_id: string,
  request_hash: InvocationRequestHash,
): Promise<string> {
  const invocationDigest = (await digestBytes(
    new TextEncoder().encode(invocation_id),
  )).slice("sha256:".length);
  return `executions/${invocationDigest}/${request_hash.slice("sha256:".length)}.json`;
}

async function digestBytes(bytes: Uint8Array): Promise<Sha256Digest> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  const hex = [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  return `sha256:${hex}`;
}

function isExecutionArtifact(value: unknown): value is ExecutionArtifact {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const artifact = value as Record<string, unknown>;
  return (
    artifact.version === 1 &&
    typeof artifact.invocation_id === "string" &&
    /^sha256:[0-9a-f]{64}$/.test(String(artifact.request_hash)) &&
    Array.isArray(artifact.evidence) &&
    Object.hasOwn(artifact, "terminal_result")
  );
}
