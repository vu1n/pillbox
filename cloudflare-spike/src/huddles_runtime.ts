import { getSandbox } from "@cloudflare/sandbox";
import { WorkerEntrypoint } from "cloudflare:workers";
import type { Env } from "./worker.js";
import {
  type CancelInvocationV2Request,
  type ExecuteInvocationV2Request,
  type GetInvocationV2Request,
} from "./codex_execution.js";
import { R2ExecutionArtifactStore } from "./execution_artifacts.js";
import {
  ExecutionService,
  OpencodeExecutionRuntime,
  type ExecutionOperationAuthorizer,
  type ExecutionRuntime,
} from "./execution_service.js";
import {
  D1ExecutionStore,
  parseManagedExecutionAllowance,
  type RelationalDatabase,
} from "./execution_store.js";
import {
  enforceHuddlesOpencodePolicy,
  type HuddlesOpencodeConfig,
} from "./huddles_policy.js";
import {
  ensureLegacySession,
  invokeLegacySession,
  validateEnsureSessionRequest,
  validateInvokeSessionRequest,
  type EnsureSessionRequest,
  type EnsureSessionResult,
  type InvokeSessionRequest,
  type InvokeSessionResult,
} from "./legacy_huddles_adapter.js";
import {
  authorizeExecutionOperation,
  requireManagedBurninBootstrap,
} from "./managed_auth.js";
import type { PillboxExecutionOperationGrantIssueResponse } from "./managed_contract.js";
import {
  RunCostMeter,
  WorkersAnalyticsEngineRunCost,
} from "./run_cost.js";
import {
  managedAdmissionPolicy,
  requireManagedAdmission,
} from "./managed_admission.js";
import { deriveSandboxRuntimeId } from "./runtime_identity.js";

export {
  deriveExecutionSessionName,
  legacyExecutionRequest,
  validateEnsureSessionRequest,
  validateInvokeSessionRequest,
} from "./legacy_huddles_adapter.js";
export type {
  CanonicalSessionRequest,
  EffectCompletionAttribution,
  EnsureSessionConflict,
  EnsureSessionRequest,
  EnsureSessionResponse,
  EnsureSessionResult,
  InvokeSessionRequest,
  InvokeSessionResult,
  SessionRef,
} from "./legacy_huddles_adapter.js";
export { canonicalJson } from "./codex_execution.js";
export type { JsonSchemaOutputFormat, JsonValue } from "./codex_execution.js";
export { isHuddlesSessionName } from "./huddles_policy.js";
export { deriveSandboxRuntimeId, sha256Hex } from "./runtime_identity.js";

/** Private same-account RPC surface for Huddles; execution/2 operation grants are mandatory. */
export class HuddlesRuntimeEntrypoint extends WorkerEntrypoint<Env> {
  async executeInvocation(
    request: ExecuteInvocationV2Request,
    authorization: PillboxExecutionOperationGrantIssueResponse,
  ) {
    return executionService(
      this.env,
      operationAuthorizer(this.env, authorization),
    ).executeInvocation(request);
  }

  async getExecutionStatus(
    request: GetInvocationV2Request,
    authorization: PillboxExecutionOperationGrantIssueResponse,
  ) {
    return executionService(
      this.env,
      operationAuthorizer(this.env, authorization),
    ).getExecutionStatus(request);
  }

  async cancelInvocation(
    request: CancelInvocationV2Request,
    authorization: PillboxExecutionOperationGrantIssueResponse,
  ) {
    return executionService(
      this.env,
      operationAuthorizer(this.env, authorization),
    ).cancelInvocation(request);
  }

  async fetch(): Promise<Response> {
    return new Response("not found\n", { status: 404 });
  }
}

/** Local-test compatibility only; this class is never exported by the production Worker. */
export class LocalLegacyRuntimeEntrypoint extends WorkerEntrypoint<Env> {
  async ensureSession(request: EnsureSessionRequest): Promise<EnsureSessionResult> {
    return ensureLegacySession(validateEnsureSessionRequest(request));
  }

  async invokeSession(request: InvokeSessionRequest): Promise<InvokeSessionResult> {
    const validated = await validateInvokeSessionRequest(request);
    requireManagedAdmission(
      managedAdmissionPolicy(this.env.MANAGED_EXECUTION_ENABLED),
    );
    const service = executionService(this.env);
    return invokeLegacySession(validated, (execution) =>
      service.executeInvocation(execution),
    );
  }

  async fetch(): Promise<Response> {
    return new Response("not found\n", { status: 404 });
  }
}

export function executionService(
  env: Env,
  authorizer?: ExecutionOperationAuthorizer,
): ExecutionService {
  requireManagedBurninBootstrap(env);
  const meter = new RunCostMeter();
  const store = new D1ExecutionStore(
    env.EXECUTION_DB as unknown as RelationalDatabase,
    meter.observeRelational,
  );
  const artifacts = new R2ExecutionArtifactStore(
    env.EXECUTION_EVIDENCE,
    meter.observeObject,
  );
  let runtime: ExecutionRuntime;
  if (env.Sandbox === undefined) {
    runtime = new UnavailableExecutionRuntime();
  } else {
    const sandboxNamespace = env.Sandbox;
    runtime = new OpencodeExecutionRuntime({
      sandboxFor: async (sessionId) =>
        getSandbox(sandboxNamespace, await deriveSandboxRuntimeId(sessionId)),
      configFor: (request) => opencodeConfig(env, request.tool_policy),
    });
  }
  return new ExecutionService(store, artifacts, runtime, {
    costMeter: meter,
    analytics:
      env.RUN_COSTS === undefined
        ? undefined
        : new WorkersAnalyticsEngineRunCost(env.RUN_COSTS),
    sandboxProfile: env.SANDBOX_PROFILE,
    admission: managedAdmissionPolicy(env.MANAGED_EXECUTION_ENABLED),
    allowance: parseManagedExecutionAllowance(
      env.MANAGED_EXECUTION_EPOCH,
      env.MANAGED_EXECUTION_LIMIT,
    ),
    authorizer,
  });
}

function operationAuthorizer(
  env: Env,
  authorization: PillboxExecutionOperationGrantIssueResponse,
): ExecutionOperationAuthorizer {
  return ({ operation, request }) =>
    authorizeExecutionOperation(env, authorization, operation, request);
}

class UnavailableExecutionRuntime implements ExecutionRuntime {
  async execute(): Promise<{
    served_model: null;
    error: { code: "runtime_unavailable"; message: string };
    evidence: readonly [];
  }> {
    return {
      served_model: null,
      error: {
        code: "runtime_unavailable",
        message: "Pillbox managed runner has no Cloudflare Sandbox binding",
      },
      evidence: [],
    };
  }

  async cancel(): Promise<void> {}
}

function opencodeConfig(
  env: Env,
  toolPolicy: "deny_all" | "runtime_default",
): { readonly config?: unknown; readonly env: Readonly<Record<string, string>> } {
  const providerEnv: Record<string, string> = {};
  for (const key of ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] as const) {
    const value = env[key];
    if (value) providerEnv[key] = value;
  }
  let config: HuddlesOpencodeConfig | undefined = env.OPENCODE_CONFIG_JSON
    ? (JSON.parse(env.OPENCODE_CONFIG_JSON) as HuddlesOpencodeConfig)
    : undefined;
  if (env.ZAI_API_KEY) {
    config ??= {};
    config.provider ??= {};
    config.provider["zai-coding-plan"] ??= {
      options: { apiKey: env.ZAI_API_KEY },
    };
  }
  if (config === undefined && Object.keys(providerEnv).length === 0) {
    throw new Error("no opencode provider configured");
  }
  return {
    config:
      toolPolicy === "deny_all"
        ? enforceHuddlesOpencodePolicy(config, toolPolicy)
        : config,
    env: providerEnv,
  };
}
