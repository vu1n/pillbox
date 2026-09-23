import type {
  CancelInvocationV2Request,
  ExecuteInvocationV2Request,
  JsonValue,
} from "./codex_execution.js";
import type { ExecutionRuntime, RuntimeTurnResult } from "./execution_service.js";
import type { DigitalOceanAllocations } from "./digitalocean_allocations.js";
import { object, type DigitalOceanApi } from "./digitalocean_client.js";
import {
  digitalOceanAdmission,
  DIGITALOCEAN_HARNESS_VERSION,
  type DigitalOceanSettings,
} from "./digitalocean_contract.js";
import {
  DIGITALOCEAN_GUEST,
  DIGITALOCEAN_LAUNCH,
  DIGITALOCEAN_POLL,
} from "./digitalocean_guest.js";
import { openRouterReasoning } from "./opencode_reasoning.js";
import { huddlesPromptTools } from "./huddles_policy.js";
import { sha256Hex } from "./runtime_identity.js";
import { inspectRawStructuredOutput } from "./structured_output.js";

export interface DigitalOceanRuntimeOptions {
  readonly api: DigitalOceanApi;
  readonly allocations: DigitalOceanAllocations;
  readonly settings: DigitalOceanSettings;
  readonly delay?: (ms: number) => Promise<void>;
  readonly now?: () => number;
}

/** One fresh sandbox and native session per sealed invocation; no vendor managed-chat memory. */
export class DigitalOceanExecutionRuntime implements ExecutionRuntime {
  private readonly options: DigitalOceanRuntimeOptions;
  constructor(options: DigitalOceanRuntimeOptions) {
    this.options = options;
  }

  preflight(request: ExecuteInvocationV2Request) {
    return digitalOceanAdmission(request, this.options.settings);
  }

  async execute(request: ExecuteInvocationV2Request): Promise<RuntimeTurnResult> {
    const unsupported = this.preflight(request);
    if (unsupported) return { served_model: null, error: unsupported, evidence: [] };
    const { allocations, settings } = this.options;
    const name = await allocationName(settings.namespace!, request.invocation_id);
    if (!(await allocations.reserve(request.invocation_id, name, settings.configId!))) {
      throw new Error(
        "DigitalOcean invocation allocation already exists; reconcile without re-execution",
      );
    }
    let turn: RuntimeTurnResult | undefined;
    let failure: unknown;
    try {
      turn = await this.runAllocated(request, name);
    } catch (cause) {
      failure = cause;
    }
    try {
      await this.cleanup(request.invocation_id);
    } catch (cause) {
      if (turn !== undefined) {
        return {
          served_model: turn.served_model,
          error: {
            code: "runtime_failed",
            message: "DigitalOcean cleanup is pending; query status to reconcile",
          },
          evidence: [
            ...turn.evidence,
            {
              type: "attention_required",
              reason: "error_stalled",
              message: "DigitalOcean cleanup is pending",
            },
          ],
        };
      }
      throw cause;
    }
    if (turn === undefined) throw failure ?? new Error("DigitalOcean execution produced no result");
    return turn;
  }

  private async runAllocated(
    request: ExecuteInvocationV2Request,
    name: string,
  ): Promise<RuntimeTurnResult> {
    const { api, allocations, settings } = this.options;
    const created = await api.create(name, settings.configId!);
    if (!(await allocations.attach(request.invocation_id, created.session_id))) {
      throw new Error("DigitalOcean allocation was cancelled during creation");
    }
    const delay =
      this.options.delay ??
      ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
    const now = this.options.now ?? Date.now;
    const readyDeadline = now() + 60_000;
    let session = created;
    for (
      let attempt = 0;
      session.status !== "SESSION_STATUS_READY" && attempt < 30 && now() < readyDeadline;
      attempt++
    ) {
      await delay(1_000);
      session = await api.get(created.session_id);
    }
    if (session.status !== "SESSION_STATUS_READY")
      throw new Error("DigitalOcean sandbox did not become ready");
    const resultPath = `/tmp/${name}.json`;
    const payload = JSON.stringify({
      text: request.rendered_input,
      model: request.execution.requested.model,
      reasoning: openRouterReasoning(request.execution.requested),
      harness_version: DIGITALOCEAN_HARNESS_VERSION,
      tools: huddlesPromptTools("deny_all"),
      output_format: request.output_format,
    });
    if ((await allocations.get(request.invocation_id))?.state !== "ready")
      throw new Error("DigitalOcean execution was cancelled before launch");
    // This launch is never retried: timeout means unknown side effects, not permission to sample again.
    const launched = await api.exec(created.session_id, [
      "python3",
      "-c",
      DIGITALOCEAN_LAUNCH,
      DIGITALOCEAN_GUEST,
      payload,
      resultPath,
    ]);
    if (launched.trim() !== "started")
      throw new Error("DigitalOcean guest launch was not acknowledged");
    const deadline = now() + 270_000;
    for (let attempt = 0; attempt < 135 && now() < deadline; attempt++) {
      const allocation = await allocations.get(request.invocation_id);
      if (allocation?.state !== "ready") throw new Error("DigitalOcean execution was cancelled");
      const result = object(
        JSON.parse(
          await api.exec(created.session_id, ["python3", "-c", DIGITALOCEAN_POLL, resultPath]),
        ),
      );
      if (result.pending !== true) return decodeDigitalOceanResult(result, request);
      await delay(2_000);
    }
    throw new Error("DigitalOcean native turn exceeded its deadline");
  }

  async cancel(request: CancelInvocationV2Request): Promise<void> {
    await this.cleanup(request.invocation_id);
  }

  /** Called after authorization on expired/terminal status reads; only provider deletion is retried. */
  async cleanup(invocationId: string): Promise<void> {
    const { allocations, api } = this.options;
    const row = await allocations.stop(
      invocationId,
      await allocationName(this.options.settings.namespace ?? "", invocationId),
    );
    if (row.state === "deleted") return;
    let id = row.provider_session_id;
    if (id === null) {
      const found = await api.find(row.name);
      // A timed-out create may still materialize later. Keep a visible stopping row;
      // absence now is not proof that no sandbox can appear.
      if (found === null)
        throw new Error("DigitalOcean creation unresolved; cleanup requires reconciliation");
      if (found.config_id !== row.config_id)
        throw new Error("DigitalOcean recovery config identity mismatch");
      id = found.session_id;
      await allocations.attach(invocationId, id);
    }
    await api.remove(id);
    await allocations.deleted(invocationId);
  }
}

async function allocationName(namespace: string, invocationId: string): Promise<string> {
  return `pbx-${(await sha256Hex(JSON.stringify([namespace, invocationId]))).slice(0, 56)}`;
}

/** Trust served identity only from the completed native assistant message, never the request. */
export function decodeDigitalOceanResult(
  value: Record<string, unknown>,
  request: ExecuteInvocationV2Request,
): RuntimeTurnResult {
  if (value.error) {
    const phase = [
      "harness_check",
      "authentication_check",
      "server_start",
      "session_create",
      "native_turn",
      "native_result_validation",
      "result_too_large",
    ].includes(String(value.error))
      ? String(value.error)
      : "unknown";
    throw new Error(`DigitalOcean guest failed during ${phase}`);
  }
  if (value.harness_version !== DIGITALOCEAN_HARNESS_VERSION)
    throw new Error("DigitalOcean harness version mismatch");
  const info = object(value.info);
  if (
    info.role !== "assistant" ||
    typeof info.id !== "string" ||
    !info.id ||
    typeof info.providerID !== "string" ||
    !info.providerID ||
    typeof info.modelID !== "string" ||
    !info.modelID ||
    !["stop", "tool-calls"].includes(String(info.finish)) ||
    typeof value.text !== "string"
  ) {
    throw new Error("DigitalOcean returned incomplete native assistant evidence");
  }
  const served = `${info.providerID}/${info.modelID}`;
  if (served.length > 256) throw new Error("DigitalOcean served-model identity exceeds limit");
  const tokens = object(info.tokens);
  const cache = object(tokens.cache);
  const count = (v: unknown): number => {
    if (typeof v !== "number" || !Number.isSafeInteger(v) || v < 0)
      throw new Error("DigitalOcean returned invalid native token usage");
    return v;
  };
  if (
    info.cost !== undefined &&
    (typeof info.cost !== "number" || !Number.isFinite(info.cost) || info.cost < 0)
  )
    throw new Error("DigitalOcean returned invalid model cost");
  const nativeEvidence: JsonValue[] = [
    {
      type: "tool_call",
      toolCallId: "pillbox:do-native-result",
      name: "pillbox.runtime_evidence",
      status: "completed",
      output: JSON.stringify({
        provider: "digitalocean",
        harness_version: value.harness_version,
        providerID: info.providerID,
        modelID: info.modelID,
        messageID: info.id,
      }),
    },
    {
      type: "usage",
      messageId: info.id,
      source: "native",
      inputTokens: count(tokens.input),
      outputTokens: count(tokens.output),
      cacheReadInputTokens: count(cache.read),
      cacheCreationInputTokens: count(cache.write),
      ...(info.cost === undefined ? {} : { costUsd: info.cost as number }),
    },
  ];
  let output: { text: string } | { json: JsonValue };
  if (request.output_format.type === "json_schema") {
    const inspected = inspectRawStructuredOutput(
      info.structured === undefined ? value.text : JSON.stringify(info.structured),
      request.output_format.schema,
    );
    if (inspected.status !== "accepted") {
      return {
        served_model: served,
        error: {
          code: "structured_output_missing",
          message: "DigitalOcean result did not satisfy the sealed output schema",
        },
        evidence: [
          ...nativeEvidence,
          {
            type: "attention_required",
            reason: "error_stalled",
            message: "structured output validation failed",
          },
        ],
      };
    }
    output = { json: JSON.parse(inspected.output) as JsonValue };
  } else {
    if (!value.text.trim()) throw new Error("DigitalOcean returned no assistant text");
    output = { text: value.text };
  }
  const text = "text" in output ? output.text : JSON.stringify(output.json);
  return {
    served_model: served,
    output,
    evidence: [
      ...nativeEvidence,
      { type: "message_start", messageId: info.id, role: "assistant" },
      { type: "message_delta", messageId: info.id, text },
      { type: "message_end", messageId: info.id },
    ],
  };
}
