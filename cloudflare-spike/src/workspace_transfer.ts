import { getSandbox, type Sandbox } from "@cloudflare/sandbox";
import { bearerToken, verifyActorToken } from "./auth.js";
import { safeHuddlesRuntimeDiagnostic } from "./huddles_policy.js";
import { deriveSandboxRuntimeId } from "./huddles_runtime.js";
import { OPENCODE_WORKSPACE_DIR } from "./opencode_turn.js";
import { workspaceExecEnv, type WorkspaceRepo } from "./workspace_repo.js";

const WORKSPACE_XFER_TIMEOUT_MS = 300_000;

export interface WorkspaceTransferEnv {
  readonly Sandbox?: DurableObjectNamespace<Sandbox>;
  readonly ACTOR_TOKEN_SECRET?: string;
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
  const token = bearerToken(request);
  if (
    env.ACTOR_TOKEN_SECRET === undefined ||
    token === null ||
    (await verifyActorToken(token, env.ACTOR_TOKEN_SECRET)) === null
  ) {
    return Response.json({ error: { code: "unauthenticated" } }, { status: 401 });
  }
  if (!env.Sandbox) {
    return Response.json(
      { error: { code: "runtime_unavailable", message: "no Sandbox binding" } },
      { status: 503 },
    );
  }

  try {
    const body = (await request.json()) as {
      sessionId?: unknown;
      workspace?: {
        repo?: WorkspaceRepo;
        password?: unknown;
        snapshot?: unknown;
      };
    };
    const sessionId = nonEmpty(body.sessionId, "sessionId");
    const workspace = body.workspace;
    if (!workspace?.repo) throw new Error("workspace.repo is required");
    const password = nonEmpty(workspace.password, "workspace.password");
    const snapshot =
      mode === "restore"
        ? nonEmpty(workspace.snapshot, "workspace.snapshot")
        : undefined;
    const sandbox = getSandbox(env.Sandbox, await deriveSandboxRuntimeId(sessionId));
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
    if (!resultSnapshot) {
      return Response.json(
        {
          error: {
            code: "workspace_transfer_failed",
            message: "workspace backup produced no snapshot handle",
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
      { status: 400 },
    );
  }
}

async function execWorkspaceTool(
  sandbox: ReturnType<typeof getSandbox>,
  command: string,
  repo: WorkspaceRepo,
  password: string,
): Promise<{ ok: boolean; detail: string; stdout: string }> {
  const execEnv = workspaceExecEnv(repo, password);
  let lastError = "";
  for (let attempt = 0; attempt < 60; attempt++) {
    try {
      const result = await sandbox.exec(command, {
        env: execEnv,
        timeout: WORKSPACE_XFER_TIMEOUT_MS,
      });
      return {
        ok: result.success,
        detail: [result.stdout, result.stderr].filter(Boolean).join("\n"),
        stdout: result.stdout ?? "",
      };
    } catch (cause) {
      lastError = String(cause);
      if (!/starting|not ready/i.test(lastError)) break;
      await new Promise((resolve) => setTimeout(resolve, 1_000));
    }
  }
  return { ok: false, detail: lastError, stdout: "" };
}

function workspaceCmd(
  mode: "restore" | "backup",
  repo: WorkspaceRepo,
  snapshot?: string,
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
  if (snapshot) args.push("--snapshot", shellQuote(snapshot));
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
