import { mkdtemp, open, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseR2Capture } from "../src/r2_capture.ts";

const MAX_CAPTURE_BYTES = 512 * 1024;
const unavailable = reason => ({ status: "unavailable", reason });

// Private operator evidence, never a field added to the immutable run report.
export async function createOperationSidecar(path, identity, repo) {
  if (typeof repo?.bucket !== "string" || !repo.bucket.length ||
      Buffer.byteLength(repo.bucket) > 1024 || typeof repo?.prefix !== "string" || !repo.prefix.length) {
    throw new Error("R2 capture requires the exact workspace bucket and nonempty prefix");
  }
  const prefix = `${repo.prefix.replace(/^\/+|\/+$/g, "")}/`;
  if (prefix === "/" || Buffer.byteLength(prefix) > 1024) {
    throw new Error("R2 capture requires a bounded nonempty normalized prefix");
  }
  const handle = await open(path, "wx", 0o600);
  const value = {
    schema_version: 1,
    capture_type: "burnin_r2_operations",
    ...identity,
    captures: {
      snapshot_finalize: unavailable("not_observed"),
      verification: unavailable("not_observed"),
    },
  };
  const save = async () => {
    const bytes = Buffer.from(`${JSON.stringify(value)}\n`);
    let offset = 0;
    while (offset < bytes.length) {
      const { bytesWritten } = await handle.write(bytes, offset, bytes.length - offset, offset);
      if (!bytesWritten) throw new Error("R2 capture sidecar write made no progress");
      offset += bytesWritten;
    }
    await handle.truncate(bytes.length);
  };
  const record = async (operation, raw, replay = false) => {
    value.captures[operation] = replay ? unavailable("replayed_without_capture") :
      parseR2Capture(raw, { operation, bucket: repo.bucket, prefix }) ?? unavailable("missing_or_invalid_capture");
    await save();
  };
  try { await save(); } catch (error) { await handle.close(); throw error; }
  return {
    recordFinalize: (raw, replay) => record("snapshot_finalize", raw, replay),
    async verify(run) {
      const directory = await mkdtemp(join(tmpdir(), "pillbox-r2-verify-"));
      const capturePath = join(directory, "capture.json");
      try {
        return await run(capturePath);
      } finally {
        let capture;
        try {
          const file = await open(capturePath, "r");
          try {
            if ((await file.stat()).size > MAX_CAPTURE_BYTES) throw new Error("capture exceeds bound");
            capture = JSON.parse(await file.readFile("utf8"));
          } finally { await file.close(); }
        } catch {
          // A missing/invalid helper sidecar is evidence loss, never zero usage.
          capture = null;
        }
        try { await record("verification", capture); }
        finally { await rm(directory, { recursive: true, force: true }); }
      }
    },
    close: () => handle.close(),
  };
}
