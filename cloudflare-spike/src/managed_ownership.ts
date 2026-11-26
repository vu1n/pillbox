import type { ManagedCapability } from "./auth.js";
import { canonicalJson } from "./codex_execution.js";
import { sha256Hex } from "./runtime_identity.js";

export type ManagedExecutionOwner = {
  // Applied migration 0004's CHECK also admits a retired owner domain; nothing writes it any more.
  readonly domain: "public_controller";
  readonly digest: `sha256:${string}`;
};

/** The durable owner of a public capability's executions; the digest is stable per subject. */
export async function publicControllerOwner(
  capability: Pick<ManagedCapability, "subject">,
): Promise<ManagedExecutionOwner> {
  const digest = await sha256Hex(
    canonicalJson({
      version: "pillbox.execution-owner/1",
      domain: "public_controller",
      subject: capability.subject,
    }),
  );
  return { domain: "public_controller", digest: `sha256:${digest}` };
}
