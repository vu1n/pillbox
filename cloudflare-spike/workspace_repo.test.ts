import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import test from "node:test";

import { signManagedCapability } from "./src/auth.ts";
import type { ManagedExecutionOwner } from "./src/managed_ownership.ts";
import type {
  ManagedExecutionAllowance,
  ManagedExecutionReservation,
  ManagedExecutionReservationStore,
  ManagedReservationClaim,
  ManagedReservationInput,
} from "./src/managed_reservation.ts";
import { workspaceExecEnv, type WorkspaceRepo } from "./src/workspace_repo.ts";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "cloudflare:workers") {
      return {
        shortCircuit: true,
        url: "data:text/javascript,export class RpcTarget {}; export class DurableObject {}; export const env = {};",
      };
    }
    if (specifier === "@cloudflare/sandbox") {
      return {
        shortCircuit: true,
        url: "data:text/javascript,export const getSandbox = () => { throw new Error('test must inject sandboxFor'); }; export class Sandbox {};",
      };
    }
    return nextResolve(
      context.parentURL?.includes("/cloudflare-spike/src/") &&
        specifier.startsWith(".") && specifier.endsWith(".js")
        ? `${specifier.slice(0, -3)}.ts`
        : specifier,
      context,
    );
  },
});

const { routeWorkspaceTransfer } = await import("./src/workspace_transfer.ts");

const repo: WorkspaceRepo = {
  endpoint: "https://account.r2.cloudflarestorage.com",
  region: "auto",
  bucket: "workspaces",
  prefix: "project/run/",
  access_key: "scoped-ak",
  secret_key: "scoped-sk",
};

test("scoped R2 credentials forward their session token to the workspace helper", () => {
  const env = workspaceExecEnv({ ...repo, session_token: "scoped-session-token" }, "repo-password");
  assert.equal(env.PILLBOX_R2_SESSION_TOKEN, "scoped-session-token");
});

test("long-lived R2 credentials omit the session-token environment variable", () => {
  const env = workspaceExecEnv(repo, "repo-password");
  assert.equal("PILLBOX_R2_SESSION_TOKEN" in env, false);
});

test("provision reserves before restore and exact retry never restores twice", async () => {
  const events: string[] = [];
  const reservations = new MemoryReservations(() => events.push("reserve"));
  let restores = 0;
  const body = provisionBody();
  const request = await signedRequest("/v2/workspaces/provision", body, "workspace_provision");
  const dependencies = {
    reservationStore: reservations,
    sandboxFor: async () => {
      events.push("sandbox");
      return {
        killAllProcesses: async () => {},
        exec: async () => {
          restores += 1;
          return { success: true, stdout: "", stderr: "" };
        },
      };
    },
    now: () => 1,
  };
  const first = await routeWorkspaceTransfer(request, transferEnv(), dependencies);
  assert.equal(first?.status, 200);
  assert.deepEqual(events.slice(0, 2), ["reserve", "sandbox"]);
  assert.equal(restores, 1);

  const retry = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/provision", body, "workspace_provision"),
    transferEnv(),
    dependencies,
  );
  assert.equal(retry?.status, 200);
  assert.equal((await retry?.json() as { disposition: string }).disposition, "reused");
  assert.equal(restores, 1);
});

test("an ambiguous post-restore failure stays provisioning and never restores again", async () => {
  let restores = 0;
  const reservations = new MemoryReservations(undefined, false);
  const dependencies = {
    reservationStore: reservations,
    sandboxFor: async () => ({
      killAllProcesses: async () => {},
      exec: async () => {
        restores += 1;
        return { success: true, stdout: "", stderr: "" };
      },
    }),
    now: () => 1,
  };
  const body = provisionBody();
  const first = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/provision", body, "workspace_provision"),
    transferEnv(),
    dependencies,
  );
  assert.equal(first?.status, 502);

  const retry = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/provision", body, "workspace_provision"),
    transferEnv(),
    dependencies,
  );
  assert.equal(retry?.status, 202);
  assert.equal(restores, 1);
});

test("provision requires prepared invocation identity before Sandbox access", async () => {
  let sandboxAccess = 0;
  const body = { ...provisionBody(), invocationId: undefined };
  const response = await routeWorkspaceTransfer(
    new Request("https://pillbox.test/v2/workspaces/provision", {
      method: "POST",
      body: JSON.stringify(body),
    }),
    transferEnv(),
    {
      reservationStore: new MemoryReservations(),
      sandboxFor: async () => {
        sandboxAccess += 1;
        throw new Error("must not reach Sandbox");
      },
    },
  );
  assert.equal(response?.status, 400);
  assert.equal(sandboxAccess, 0);
});

test("finalize requires an existing reserved session before Sandbox access", async () => {
  let sandboxAccess = 0;
  const body = { sessionId: "missing-session", workspace: provisionBody().workspace };
  const response = await routeWorkspaceTransfer(
    await signedRequest("/v2/workspaces/finalize", body, "workspace_finalize"),
    transferEnv(),
    {
      reservationStore: new MemoryReservations(),
      sandboxFor: async () => {
        sandboxAccess += 1;
        throw new Error("must not reach Sandbox");
      },
    },
  );
  assert.equal(response?.status, 404);
  assert.equal(sandboxAccess, 0);
});

const capabilitySecret = "workspace-transfer-test-secret";

function provisionBody() {
  return {
    sessionId: "session-a",
    invocationId: "invocation-a",
    executionRequestHash: `sha256:${"c".repeat(64)}`,
    workspace: {
      repo,
      password: "repo-password",
      snapshot: "e".repeat(64),
    },
  };
}

async function signedRequest(
  path: string,
  body: unknown,
  operation: "workspace_provision" | "workspace_finalize",
): Promise<Request> {
  const encoded = JSON.stringify(body);
  const requestHash = `sha256:${[...new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(encoded)))]
    .map((byte) => byte.toString(16).padStart(2, "0")).join("")}` as const;
  const value = body as { sessionId: string; invocationId?: string };
  const token = await signManagedCapability({
    version: 1,
    subject: "controller:test",
    audience: "pillbox-managed",
    expires_at_ms: Date.now() + 60_000,
    operation,
    request_sha256: requestHash,
    session_id: value.sessionId,
    ...(value.invocationId === undefined ? {} : { invocation_id: value.invocationId }),
  }, capabilitySecret);
  return new Request(`https://pillbox.test${path}`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}` },
    body: encoded,
  });
}

function transferEnv() {
  return {
    EXECUTION_DB: {} as D1Database,
    MANAGED_CAPABILITY_SECRET: capabilitySecret,
    MANAGED_EXECUTION_EPOCH: "burnin-test",
    MANAGED_EXECUTION_LIMIT: "2",
  };
}

class MemoryReservations implements ManagedExecutionReservationStore {
  private record: ManagedExecutionReservation | null = null;
  private readonly onClaim: () => void;
  private readonly readyTransition: boolean;
  constructor(onClaim: (() => void) | undefined = undefined, readyTransition = true) {
    this.onClaim = onClaim ?? (() => {});
    this.readyTransition = readyTransition;
  }
  async claimProvision(input: ManagedReservationInput, allowance: ManagedExecutionAllowance): Promise<ManagedReservationClaim> {
    this.onClaim();
    if (this.record !== null) return { kind: "reused", record: this.record };
    this.record = {
      invocation_id: input.invocation_id,
      session_id: input.session_id,
      owner: input.owner,
      execution_request_hash: input.execution_request_hash,
      source: "workspace_provision",
      provision_request_digest: input.provision_request_digest ?? null,
      status: "provisioning",
      error_code: null,
      allowance_epoch: allowance.deployment_epoch,
      allowance_limit: allowance.execution_limit,
      created_at_ms: input.now_ms,
      updated_at_ms: input.now_ms,
    };
    return { kind: "created", record: this.record };
  }
  async claimExecution(): Promise<ManagedReservationClaim> { throw new Error("unused"); }
  async getReady(): Promise<ManagedExecutionReservation | null> { return null; }
  async getSessionOwner(): Promise<ManagedExecutionOwner | null> { return this.record?.owner ?? null; }
  async markReady(): Promise<boolean> {
    if (this.record === null || !this.readyTransition) return false;
    this.record = { ...this.record, status: "ready" };
    return true;
  }
  async markFailed(input: { error_code: string }): Promise<boolean> {
    if (this.record === null) return false;
    this.record = { ...this.record, status: "failed", error_code: input.error_code };
    return true;
  }
  async getAllowance(allowance: ManagedExecutionAllowance) {
    return { ...allowance, reserved_executions: this.record === null ? 0 : 1 };
  }
}
