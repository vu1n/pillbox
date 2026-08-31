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

const WORKSPACE_XFER_TIMEOUT_MS = 300_000;

export interface WorkspaceTransferEnv {
  readonly Sandbox?: DurableObjectNamespace<Sandbox>;
  readonly MANAGED_CAPABILITY_SECRET?: string;
}

export async function routeWorkspaceTransfer(
  request: Request,
  env: WorkspaceTransferEnv,
): Promise<Response | null> {
  const path = new URL(request.url).pathname;
  const mode =
    path === "/v2/workspaces/provision"
      ? "restore"
      : path === "/v2/workspaces/finalize"
        ? "backup"
        : null;
  if (mode === null) return null;
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
      workspace?: {
        repo?: WorkspaceRepo;
        password?: unknown;
        snapshot?: unknown;
      };
    };
    const sessionId = nonEmpty(body.sessionId, "sessionId");
    const token = bearerToken(request);
    const operation = mode === "restore" ? "workspace_provision" : "workspace_finalize";
    if (
      env.MANAGED_CAPABILITY_SECRET === undefined ||
      token === null ||
      (await verifyManagedCapability(token, env.MANAGED_CAPABILITY_SECRET, {
        operation,
        request_sha256: decoded.sha256,
        session_id: sessionId,
      })) === null
    ) {
      return Response.json({ error: { code: "unauthenticated" } }, { status: 401 });
    }
    if (!env.Sandbox) {
      return Response.json(
        { error: { code: "runtime_unavailable", message: "no Sandbox binding" } },
        { status: 503 },
      );
    }
    const workspace = body.workspace;
    if (!workspace?.repo) throw new Error("workspace.repo is required");
    validateR2Repo(workspace.repo);
    const password = nonEmpty(workspace.password, "workspace.password");
    const snapshot = snapshotHandle(workspace.snapshot, "workspace.snapshot");
    const sandbox = getSandbox(env.Sandbox, await deriveSandboxRuntimeId(sessionId));
    if (mode === "backup") {
      // Finalize credentials must never overlap a prompt-controlled process.
      await sandbox.killAllProcesses();
    }
    const result = await execWorkspaceTool(
      sandbox,
      workspaceCmd(mode, workspace.repo, snapshot),
      workspace.repo,
      password,
    );
    if (!result.ok) {
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
    if (mode === "restore") return Response.json({ ok: true });
    const resultSnapshot = result.stdout.trim().split("\n").filter(Boolean).pop();
    if (!resultSnapshot || !/^[0-9a-f]{64}$/.test(resultSnapshot)) {
      return Response.json(
        {
          error: {
            code: "workspace_transfer_failed",
            message: "workspace backup produced no canonical snapshot handle",
          },
        },
        { status: 502 },
      );
    }
    return Response.json({ resultSnapshot });
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

function snapshotHandle(value: unknown, name: string): string {
  const handle = nonEmpty(value, name);
  if (!/^[0-9a-f]{64}$/.test(handle)) {
    throw new Error(`${name} must be a 64-character lowercase hex snapshot id`);
  }
  return handle;
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
  sandbox: ReturnType<typeof getSandbox>,
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
