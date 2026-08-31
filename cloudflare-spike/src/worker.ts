import type { Sandbox } from "@cloudflare/sandbox";
import { HuddlesRuntimeEntrypoint, executionService } from "./huddles_runtime.js";
import {
  bearerToken,
  verifyManagedCapability,
  type ManagedOperation,
} from "./auth.js";
import { safeHuddlesRuntimeDiagnostic } from "./huddles_policy.js";
import {
  readBoundedJsonWithDigest,
  RequestBodyTooLargeError,
} from "./request_body.js";
import { routeWorkspaceTransfer } from "./workspace_transfer.js";

// Named entrypoint: Huddles reaches ensureSession through a same-account
// service-binding RPC. The default fetch handler below never routes that method.
export { HuddlesRuntimeEntrypoint };
// Re-export the SDK's container-owning DO so wrangler can bind it.
export { Sandbox } from "@cloudflare/sandbox";

export interface Env {
  EXECUTION_DB: D1Database;
  EXECUTION_EVIDENCE: R2Bucket;
  RUN_COSTS?: AnalyticsEngineDataset;
  // The Sandbox SDK's container DO — OPTIONAL: present only in the container
  // config (wrangler.container.toml). Absent in the free/§0-only deploy.
  Sandbox?: DurableObjectNamespace<Sandbox>;
  // HMAC issuer secret for short-lived public controller capabilities bound to
  // exact request bytes, operation, and resource. Huddles reaches the private
  // service binding and does not use this public bearer-token surface.
  MANAGED_CAPABILITY_SECRET?: string;

  // opencode provider auth + model for the consume path (driveAgent). Set via
  // `wrangler secret put` / `.dev.vars`; consumed by createOpencodeServer
  // (managed-tier Milestone 2 — the managed secret store, NOT our MITM vault).
  // Known provider keys are passed through as env so opencode auto-detects them.
  ANTHROPIC_API_KEY?: string;
  OPENAI_API_KEY?: string;
  // Z.AI GLM coding-plan subscription key — wired into opencode's `zai-coding-plan`
  // provider (a config overlay, since it isn't a standard-env auto-detect provider).
  ZAI_API_KEY?: string;
  // Full opencode `config` JSON (a provider block with an apiKey, or a CF AI
  // Gateway) — an alternative to / override of the key env vars above.
  OPENCODE_CONFIG_JSON?: string;
  // Default model (`provider/modelID`) when an /input doesn't carry one.
  OPENCODE_MODEL?: string;
  SANDBOX_PROFILE?: string;
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const executionResponse = await routeExecutionRequest(req, env);
    if (executionResponse !== null) return executionResponse;
    const workspaceResponse = await routeWorkspaceTransfer(req, env);
    if (workspaceResponse !== null) return workspaceResponse;
    return new Response("not found\n", { status: 404 });
  },
};

async function routeExecutionRequest(
  request: Request,
  env: Env,
): Promise<Response | null> {
  const path = new URL(request.url).pathname;
  const operation: ManagedOperation | null =
    path === "/v2/executions"
      ? "execute"
      : path === "/v2/executions/status"
        ? "status"
        : path === "/v2/executions/cancel"
          ? "cancel"
          : null;
  if (operation === null) return null;
  if (request.method !== "POST") {
    return new Response("method not allowed\n", {
      status: 405,
      headers: { allow: "POST" },
    });
  }
  try {
    const decoded = await readBoundedJsonWithDigest(request);
    const body = decoded.value;
    const scope = executionCapabilityScope(operation, body, decoded.sha256);
    const token = bearerToken(request);
    if (
      env.MANAGED_CAPABILITY_SECRET === undefined ||
      token === null ||
      (await verifyManagedCapability(
        token,
        env.MANAGED_CAPABILITY_SECRET,
        scope,
      )) === null
    ) {
      return Response.json({ error: { code: "unauthenticated" } }, { status: 401 });
    }
    if (
      operation === "execute" &&
      typeof body === "object" &&
      body !== null &&
      "tool_policy" in body &&
      body.tool_policy !== "deny_all"
    ) {
      return Response.json(
        {
          error: {
            code: "unsupported_policy",
            message: "public managed execution requires tool_policy 'deny_all'",
          },
        },
        { status: 400 },
      );
    }
    const service = executionService(env);
    const result =
      operation === "execute"
        ? await service.executeInvocation(body)
        : operation === "status"
          ? await service.getExecutionStatus(body)
          : await service.cancelInvocation(body);
    return Response.json(result, {
      status: result.status === "running" ? 202 : result.status === "conflict" ? 409 : 200,
    });
  } catch (cause) {
    const code =
      typeof cause === "object" && cause !== null && "code" in cause
        ? String(cause.code)
        : "managed_execution_failed";
    return Response.json(
      {
        error: {
          code,
          message: safeHuddlesRuntimeDiagnostic(cause),
        },
      },
      {
        status:
          cause instanceof RequestBodyTooLargeError
            ? 413
            : code === "execution_not_found"
              ? 404
              : 400,
      },
    );
  }
}

function executionCapabilityScope(
  operation: ManagedOperation,
  value: unknown,
  request_sha256: `sha256:${string}`,
): {
  readonly operation: ManagedOperation;
  readonly request_sha256: `sha256:${string}`;
  readonly session_id?: string;
  readonly invocation_id?: string;
} {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("request must be an object");
  }
  const body = value as Record<string, unknown>;
  const invocationId = nonEmptyScope(body.invocation_id, "invocation_id");
  if (operation !== "execute") {
    return { operation, request_sha256, invocation_id: invocationId };
  }
  if (
    typeof body.session_ref !== "object" ||
    body.session_ref === null ||
    Array.isArray(body.session_ref)
  ) {
    throw new Error("session_ref must be an object");
  }
  const sessionId = nonEmptyScope(
    (body.session_ref as Record<string, unknown>).session_id,
    "session_ref.session_id",
  );
  return {
    operation,
    request_sha256,
    session_id: sessionId,
    invocation_id: invocationId,
  };
}

function nonEmptyScope(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} must be a non-empty string`);
  }
  return value;
}
