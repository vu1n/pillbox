import { getSandbox } from "@cloudflare/sandbox";
import type { RequestedModelProfile } from "./codex_execution.js";
import { D1DigitalOceanAllocations } from "./digitalocean_allocations.js";
import { DigitalOceanClient } from "./digitalocean_client.js";
import { DigitalOceanExecutionRuntime } from "./digitalocean_runtime.js";
import { R2ExecutionArtifactStore } from "./execution_artifacts.js";
import {
  ExecutionService,
  OpencodeExecutionRuntime,
  type ExecutionOperationAuthorizer,
  type ExecutionRuntime,
} from "./execution_service.js";
import {
  D1ExecutionStore,
  type RelationalDatabase,
} from "./execution_store.js";
import { managedAdmissionPolicy } from "./managed_admission.js";
import {
  D1ManagedExecutionReservationStore,
  parseManagedExecutionAllowance,
} from "./managed_reservation.js";
import { ManagedExecutionRuntime } from "./managed_runtime.js";
import { openRouterReasoning } from "./opencode_reasoning.js";
import {
  RunCostMeter,
  WorkersAnalyticsEngineRunCost,
} from "./run_cost.js";
import { deriveSandboxRuntimeId } from "./runtime_identity.js";
import {
  enforceOpencodeToolPolicy,
  type OpencodeConfig,
  type ToolPolicy,
} from "./tool_policy.js";
import type { Env } from "./worker.js";

/** Compose the D1/R2/runtime execution service for one already-authorized caller. */
export function executionService(
  env: Env,
  authorizer: ExecutionOperationAuthorizer,
): ExecutionService {
  const meter = new RunCostMeter();
  const store = new D1ExecutionStore(
    env.EXECUTION_DB as unknown as RelationalDatabase,
    meter.observeRelational,
  );
  const reservations = new D1ManagedExecutionReservationStore(
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
      configFor: (request) => opencodeConfig(env, request.tool_policy, request.execution.requested),
    });
  }
  runtime = new ManagedExecutionRuntime(runtime, new DigitalOceanExecutionRuntime({
    api: new DigitalOceanClient(env.DIGITALOCEAN_API_TOKEN ?? ""),
    allocations: new D1DigitalOceanAllocations(env.EXECUTION_DB as unknown as RelationalDatabase, meter.observeRelational),
    settings: {
      enabled: env.DIGITALOCEAN_EXECUTION_ENABLED,
      token: env.DIGITALOCEAN_API_TOKEN,
      namespace: env.DIGITALOCEAN_ALLOCATION_NAMESPACE,
      configId: env.DIGITALOCEAN_AGENT_CONFIG_ID,
    },
  }));
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
    reservations,
    authorizer,
  });
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
  toolPolicy: ToolPolicy,
  requested: RequestedModelProfile,
): { readonly config?: unknown; readonly env: Readonly<Record<string, string>> } {
  const providerEnv: Record<string, string> = {};
  for (const key of ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENROUTER_API_KEY"] as const) {
    const value = env[key];
    if (value) providerEnv[key] = value;
  }
  let config: OpencodeConfig | undefined = env.OPENCODE_CONFIG_JSON
    ? (JSON.parse(env.OPENCODE_CONFIG_JSON) as OpencodeConfig)
    : undefined;
  if (env.ZAI_API_KEY) {
    config ??= {};
    config.provider ??= {};
    config.provider["zai-coding-plan"] ??= {
      options: { apiKey: env.ZAI_API_KEY },
    };
  }
  const reasoning = openRouterReasoning(requested);
  if (reasoning) {
    config ??= {};
    config.provider ??= {};
    const provider = config.provider.openrouter ?? {};
    config.provider.openrouter = {
      ...provider,
      models: {
        ...provider.models,
        [requested.model]: { variants: { [reasoning.variant]: reasoning.options } },
      },
    };
  }
  if (config === undefined && Object.keys(providerEnv).length === 0) {
    throw new Error("no opencode provider configured");
  }
  return {
    config: enforceOpencodeToolPolicy(config, toolPolicy),
    env: providerEnv,
  };
}
