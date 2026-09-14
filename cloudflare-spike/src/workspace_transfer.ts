import { getSandbox, type Sandbox } from "@cloudflare/sandbox";
import { bearerToken, verifyManagedCapability } from "./auth.js";
import { safeHuddlesRuntimeDiagnostic } from "./huddles_policy.js";
import { deriveSandboxRuntimeId } from "./runtime_identity.js";
import { OPENCODE_WORKSPACE_DIR } from "./opencode_turn.js";
import {
  readBoundedJsonWithDigest,
  RequestBodyTooLargeError,
} from "./request_body.js";
import { workspaceExecEnv, type WorkspaceRepo } from "./workspace_repo.js";
import {
  publicControllerOwner,
  type ManagedExecutionOwner,
} from "./managed_ownership.js";
import {
  D1ManagedExecutionReservationStore,
  ManagedExecutionAllowanceError,
  ManagedReservationAccessError,
  parseManagedExecutionAllowance,
  type ManagedReservationClaim,
  type ManagedExecutionReservationStore,
} from "./managed_reservation.js";
import {
  D1WorkspaceFinalizeStore,
  workspaceFinalizeIdentity,
  type WorkspaceFinalizeRecord,
  type WorkspaceFinalizeStore,
} from "./workspace_finalize.js";

const WORKSPACE_XFER_TIMEOUT_MS = 300_000;

export interface WorkspaceTransferEnv {
  readonly Sandbox?: DurableObjectNamespace<Sandbox>;
  readonly EXECUTION_DB: D1Database;
  readonly MANAGED_CAPABILITY_SECRET?: string;
  readonly MANAGED_EXECUTION_EPOCH?: string;
  readonly MANAGED_EXECUTION_LIMIT?: string;
}

type WorkspaceSandbox = Pick<ReturnType<typeof getSandbox>, "killAllProcesses" | "exec">;

export interface WorkspaceTransferDependencies {
  readonly finalizeStore?: WorkspaceFinalizeStore;
  readonly reservationStore?: ManagedExecutionReservationStore;
  readonly sandboxFor?: (sessionId: string) => Promise<WorkspaceSandbox>;
  readonly now?: () => number;
}

export async function routeWorkspaceTransfer(
  request: Request,
  env: WorkspaceTransferEnv,
  dependencies: WorkspaceTransferDependencies = {},
): Promise<Response | null> {
  const path = new URL(request.url).pathname;
  const mode =
    path === "/v2/workspaces/provision"
      ? "restore"
      : path === "/v2/workspaces/finalize"
        ? "backup"
        : null;
  const finalizeStatus = path === "/v2/workspaces/finalize/status";
  if (mode === null && !finalizeStatus) return null;
  if (request.method !== "POST") {
    return new Response("method not allowed\n", {
      status: 405,
      headers: { allow: "POST" },
    });
  }
  try {
    const decoded = await readBoundedJsonWithDigest(request);
    const body = decoded.value as {
      sessionId?: unknown;
      invocationId?: unknown;
      executionRequestHash?: unknown;
      workspace?: {
        repo?: WorkspaceRepo;
        password?: unknown;
        snapshot?: unknown;
      };
    };
    const sessionId = nonEmpty(body.sessionId, "sessionId");
    const token = bearerToken(request);
    const operation = mode === "restore" ? "workspace_provision" : "workspace_finalize";
    const invocationId = mode === "restore"
      ? nonEmpty(body.invocationId, "invocationId")
      : undefined;
    const capability =
      env.MANAGED_CAPABILITY_SECRET === undefined || token === null
        ? null
        : await verifyManagedCapability(token, env.MANAGED_CAPABILITY_SECRET, {
            operation,
            request_sha256: decoded.sha256,
            session_id: sessionId,
            invocation_id: invocationId,
          });
    if (capability === null) {
      return Response.json({ error: { code: "unauthenticated" } }, { status: 401 });
    }
    const workspace = body.workspace;
    if (!workspace?.repo) throw new Error("workspace.repo is required");
    validateR2Repo(workspace.repo);
    const password = nonEmpty(workspace.password, "workspace.password");
    const snapshot = snapshotHandle(workspace.snapshot, "workspace.snapshot");
    const reservations = reservationStore(env, dependencies);
    if (mode === "restore") {
      const executionRequestHash = sha256Digest(
        body.executionRequestHash,
        "executionRequestHash",
      );
      const allowance = parseManagedExecutionAllowance(
        env.MANAGED_EXECUTION_EPOCH,
        env.MANAGED_EXECUTION_LIMIT,
      );
      if (allowance === null) {
        return Response.json(
          { error: { code: "managed_disabled", message: "managed execution allowance is not configured" } },
          { status: 503 },
        );
      }
      const owner = await publicControllerOwner(capability);
      let claim: ManagedReservationClaim;
      try {
        claim = await reservations.claimProvision(
          {
            invocation_id: invocationId!,
            session_id: sessionId,
            owner,
            execution_request_hash: executionRequestHash,
            source: "workspace_provision",
            provision_request_digest: decoded.sha256,
            now_ms: (dependencies.now ?? Date.now)(),
          },
          allowance,
        );
      } catch (cause) {
        if (cause instanceof ManagedExecutionAllowanceError) {
          return Response.json(
            { error: { code: cause.code, message: cause.message } },
            { status: 503 },
          );
        }
        if (cause instanceof ManagedReservationAccessError) {
          return Response.json(
            { error: { code: "workspace_provision_conflict" } },
            { status: 409 },
          );
        }
        throw cause;
      }
      if (claim.kind === "conflict") {
        return Response.json(
          { error: { code: "workspace_provision_conflict" } },
          { status: 409 },
        );
      }
      if (claim.kind === "reused") {
        if (claim.record.status === "ready") {
          return Response.json({ ok: true, disposition: "reused" });
        }
        if (claim.record.status === "provisioning") {
          return Response.json(
            { status: "provisioning", disposition: "reused" },
            { status: 202 },
          );
        }
        return Response.json(
          { error: { code: claim.record.error_code ?? "workspace_transfer_failed" } },
          { status: 502 },
        );
      }
      if (!env.Sandbox && !dependencies.sandboxFor) {
        await reservations.markFailed({
          invocation_id: invocationId!,
          owner,
          error_code: "runtime_unavailable",
          now_ms: (dependencies.now ?? Date.now)(),
        });
        return Response.json(
          { error: { code: "runtime_unavailable", message: "no Sandbox binding" } },
          { status: 503 },
        );
      }
      const sandbox = dependencies.sandboxFor
        ? await dependencies.sandboxFor(sessionId)
        : getSandbox(env.Sandbox!, await deriveSandboxRuntimeId(sessionId));
      const result = await execWorkspaceTool(
        sandbox,
        workspaceCmd("restore", workspace.repo, snapshot),
        workspace.repo,
        password,
      );
      if (!result.ok) {
        await reservations.markFailed({
          invocation_id: invocationId!,
          owner,
          error_code: "workspace_transfer_failed",
          now_ms: (dependencies.now ?? Date.now)(),
        });
        return Response.json(
          {
            error: {
              code: "workspace_transfer_failed",
              message: redact(result.detail),
            },
          },
          { status: 502 },
        );
      }
      if (!(await reservations.markReady({
        invocation_id: invocationId!,
        owner,
        now_ms: (dependencies.now ?? Date.now)(),
      }))) {
        return Response.json(
          { error: { code: "workspace_provision_interrupted" } },
          { status: 502 },
        );
      }
      return Response.json({ ok: true, disposition: "created" });
    }
    const targetOwner = await reservations.getSessionOwner(sessionId);
    if (targetOwner === null) {
      return Response.json(
        { error: { code: "workspace_session_not_found" } },
        { status: 404 },
      );
    }
    if (finalizeStatus) {
      return finalizeStatusResponse(
        await finalizeStore(env, dependencies).get(sessionId, targetOwner),
        await workspaceFinalizeIdentity(sessionId, body),
      );
    }
    if (!env.Sandbox && !dependencies.sandboxFor) {
      return Response.json(
        { error: { code: "runtime_unavailable", message: "no Sandbox binding" } },
        { status: 503 },
      );
    }
    const sandbox = dependencies.sandboxFor
      ? await dependencies.sandboxFor(sessionId)
      : getSandbox(env.Sandbox!, await deriveSandboxRuntimeId(sessionId));
    if (mode === "backup") {
      return finalizeWorkspace({
        sandbox,
        store: finalizeStore(env, dependencies),
        sessionId,
        body,
        repo: workspace.repo,
        password,
        snapshot,
        targetOwner,
        now: dependencies.now ?? Date.now,
      });
    }
    throw new Error("unreachable workspace transfer mode");
  } catch (cause) {
    return Response.json(
      {
        error: {
          code: "invalid_workspace_transfer",
          message: safeHuddlesRuntimeDiagnostic(cause),
        },
      },
      { status: cause instanceof RequestBodyTooLargeError ? 413 : 400 },
    );
  }
}

async function finalizeWorkspace(input: {
  readonly sandbox: WorkspaceSandbox;
  readonly store: WorkspaceFinalizeStore;
  readonly sessionId: string;
  readonly body: unknown;
  readonly repo: WorkspaceRepo;
  readonly password: string;
  readonly snapshot: string;
  readonly targetOwner: ManagedExecutionOwner;
  readonly now: () => number;
}): Promise<Response> {
  const identity = await workspaceFinalizeIdentity(input.sessionId, input.body);
  const claimed = await input.store.claim({
    ...identity,
    session_id: input.sessionId,
    target_owner: input.targetOwner,
    now_ms: input.now(),
  });
  if (claimed.kind === "conflict") {
    return Response.json(
      { error: { code: "workspace_finalize_conflict", message: "session was finalized with another canonical request" } },
      { status: 409 },
    );
  }
  if (claimed.kind === "reused") return finalizeReplayResponse(claimed.record);

  try {
    // The durable running claim is the point of no return. A crash after this
    // line leaves observable status and exact retries never repeat side effects.
    await input.sandbox.killAllProcesses();
    const result = await execWorkspaceTool(
      input.sandbox,
      workspaceCmd("backup", input.repo, input.snapshot),
      input.repo,
      input.password,
    );
    if (!result.ok) throw new Error(redact(result.detail));
    const resultSnapshot = result.stdout.trim().split("\n").filter(Boolean).pop();
    if (!resultSnapshot || !/^[0-9a-f]{64}$/.test(resultSnapshot)) {
      throw new Error("workspace backup produced no canonical snapshot handle");
    }
    if (!(await input.store.complete({
      finalize_id: identity.finalize_id,
      target_owner: input.targetOwner,
      result_snapshot: resultSnapshot,
      now_ms: input.now(),
    }))) {
      throw new Error("workspace finalize completion lost its durable owner");
    }
    return Response.json({
      status: "completed",
      disposition: "created",
      finalizeId: identity.finalize_id,
      requestDigest: identity.request_digest,
      resultSnapshot,
    });
  } catch (cause) {
    const message = redact(safeHuddlesRuntimeDiagnostic(cause));
    await input.store.fail({
      finalize_id: identity.finalize_id,
      target_owner: input.targetOwner,
      error_code: "workspace_transfer_failed",
      error_message: message,
      now_ms: input.now(),
    });
    return Response.json(
      { error: { code: "workspace_transfer_failed", message } },
      { status: 502 },
    );
  }
}

function finalizeStatusResponse(
  record: WorkspaceFinalizeRecord | null,
  identity: { readonly finalize_id: `sha256:${string}`; readonly request_digest: `sha256:${string}` },
): Response {
  if (record === null) {
    return Response.json({ error: { code: "workspace_finalize_not_found" } }, { status: 404 });
  }
  if (record.finalize_id !== identity.finalize_id || record.request_digest !== identity.request_digest) {
    return Response.json({ error: { code: "workspace_finalize_conflict" } }, { status: 409 });
  }
  return finalizeReplayResponse(record);
}

function finalizeReplayResponse(record: WorkspaceFinalizeRecord): Response {
  const base = {
    status: record.status,
    disposition: "reused",
    finalizeId: record.finalize_id,
    requestDigest: record.request_digest,
  };
  if (record.status === "running") return Response.json(base, { status: 202 });
  if (record.status === "completed") {
    return Response.json({ ...base, resultSnapshot: record.result_snapshot });
  }
  return Response.json(
    { ...base, error: { code: record.error_code, message: record.error_message } },
    { status: 502 },
  );
}

function finalizeStore(
  env: WorkspaceTransferEnv,
  dependencies: WorkspaceTransferDependencies,
): WorkspaceFinalizeStore {
  return dependencies.finalizeStore ?? new D1WorkspaceFinalizeStore(env.EXECUTION_DB);
}

function reservationStore(
  env: WorkspaceTransferEnv,
  dependencies: WorkspaceTransferDependencies,
): ManagedExecutionReservationStore {
  return dependencies.reservationStore ?? new D1ManagedExecutionReservationStore(env.EXECUTION_DB);
}

function snapshotHandle(value: unknown, name: string): string {
  const handle = nonEmpty(value, name);
  if (!/^[0-9a-f]{64}$/.test(handle)) {
    throw new Error(`${name} must be a 64-character lowercase hex snapshot id`);
  }
  return handle;
}

function sha256Digest(value: unknown, name: string): `sha256:${string}` {
  const digest = nonEmpty(value, name);
  if (!/^sha256:[0-9a-f]{64}$/.test(digest)) {
    throw new Error(`${name} must be a lowercase sha256 digest`);
  }
  return digest as `sha256:${string}`;
}

function validateR2Repo(repo: WorkspaceRepo): void {
  const endpoint = new URL(nonEmpty(repo.endpoint, "workspace.repo.endpoint"));
  if (
    endpoint.protocol !== "https:" ||
    endpoint.username.length > 0 ||
    endpoint.password.length > 0 ||
    endpoint.port.length > 0 ||
    endpoint.pathname !== "/" ||
    endpoint.search.length > 0 ||
    endpoint.hash.length > 0 ||
    !/^[a-z0-9-]+\.r2\.cloudflarestorage\.com$/i.test(endpoint.hostname)
  ) {
    throw new Error("workspace.repo.endpoint must be an HTTPS Cloudflare R2 origin");
  }
  nonEmpty(repo.region, "workspace.repo.region");
  nonEmpty(repo.bucket, "workspace.repo.bucket");
  nonEmpty(repo.prefix, "workspace.repo.prefix");
  nonEmpty(repo.access_key, "workspace.repo.access_key");
  nonEmpty(repo.secret_key, "workspace.repo.secret_key");
  if (repo.session_token !== undefined) {
    nonEmpty(repo.session_token, "workspace.repo.session_token");
  }
}

async function execWorkspaceTool(
  sandbox: WorkspaceSandbox,
  command: string,
  repo: WorkspaceRepo,
  password: string,
): Promise<{ ok: boolean; detail: string; stdout: string }> {
  const execEnv = workspaceExecEnv(repo, password);
  const deadline = Date.now() + WORKSPACE_XFER_TIMEOUT_MS;
  let lastError = "";
  while (Date.now() < deadline) {
    try {
      const result = await sandbox.exec(command, {
        env: execEnv,
        timeout: Math.max(1, deadline - Date.now()),
      });
      return {
        ok: result.success,
        detail: [result.stdout, result.stderr].filter(Boolean).join("\n"),
        stdout: result.stdout ?? "",
      };
    } catch (cause) {
      lastError = String(cause);
      if (!/starting|not ready/i.test(lastError)) break;
      const remaining = deadline - Date.now();
      if (remaining <= 0) break;
      await new Promise((resolve) =>
        setTimeout(resolve, Math.min(1_000, remaining)),
      );
    }
  }
  return { ok: false, detail: lastError, stdout: "" };
}

function workspaceCmd(
  mode: "restore" | "backup",
  repo: WorkspaceRepo,
  snapshot: string,
): string {
  const args = [
    "/usr/local/bin/pillbox",
    "workspace",
    mode,
    "--endpoint",
    shellQuote(repo.endpoint),
    "--bucket",
    shellQuote(repo.bucket),
    "--region",
    shellQuote(repo.region),
    "--prefix",
    shellQuote(repo.prefix),
  ];
  args.push(
    mode === "restore" ? "--snapshot" : "--parent",
    shellQuote(snapshot),
  );
  args.push("--target", shellQuote(OPENCODE_WORKSPACE_DIR));
  return args.join(" ");
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

function nonEmpty(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} must be a non-empty string`);
  }
  return value;
}

function redact(detail: string): string {
  return detail.length > 2_000 ? `${detail.slice(0, 2_000)}…` : detail;
}
