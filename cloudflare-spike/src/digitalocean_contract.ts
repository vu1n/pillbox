import type { ExecuteInvocationV2Request } from "./codex_execution.js";
import { openRouterReasoningAdmission } from "./opencode_reasoning.js";

/** Existing transport identity is hashed and signed by execution/2; omission still means CF. */
export const DIGITALOCEAN_TRANSPORT = "digitalocean-sandbox-exec";
export const DIGITALOCEAN_ADAPTER_REVISION = "pillbox/digitalocean/1";
export const DIGITALOCEAN_HARNESS_VERSION = "1.18.31";
export const DIGITALOCEAN_MAX_INPUT_BYTES = 32 * 1024;
export const DIGITALOCEAN_MAX_RESULT_BYTES = 128 * 1024;

export interface DigitalOceanSettings {
  readonly enabled?: string;
  readonly token?: string;
  readonly namespace?: string;
  /** Operator-owned immutable config: OpenCode, OpenRouter secret, explicit egress, idle timeout. */
  readonly configId?: string;
}

export function digitalOceanAdmission(
  request: ExecuteInvocationV2Request,
  settings: DigitalOceanSettings,
):
  | { code: "managed_disabled" | "unsupported_execution" | "unsupported_policy"; message: string }
  | undefined {
  if (settings.enabled !== "1")
    return { code: "managed_disabled", message: "DigitalOcean execution is disabled" };
  if (
    !settings.namespace ||
    !settings.token ||
    !settings.configId ||
    !/^[0-9a-f-]{36}$/.test(settings.configId)
  ) {
    return {
      code: "managed_disabled",
      message: "DigitalOcean namespace, token and immutable agent config are required",
    };
  }
  const e = request.execution;
  if (
    e.transport.harness !== "opencode" ||
    e.transport.transport !== DIGITALOCEAN_TRANSPORT ||
    e.transport.harness_version !== DIGITALOCEAN_HARNESS_VERSION ||
    e.transport.adapter_revision !== DIGITALOCEAN_ADAPTER_REVISION ||
    e.placement !== "managed_container" ||
    e.requested.provider !== "openrouter"
  ) {
    return {
      code: "unsupported_execution",
      message: "DigitalOcean requires the pinned OpenCode/OpenRouter managed execution profile",
    };
  }
  const unsupportedReasoning = openRouterReasoningAdmission(e.requested);
  if (unsupportedReasoning) return unsupportedReasoning;
  if (request.tool_policy !== "deny_all")
    return { code: "unsupported_policy", message: "DigitalOcean supports deny_all only" };
  if (
    new TextEncoder().encode(request.rendered_input).byteLength > DIGITALOCEAN_MAX_INPUT_BYTES ||
    new TextEncoder().encode(JSON.stringify(request.output_format)).byteLength > 16 * 1024 ||
    new TextEncoder().encode(
      JSON.stringify({ text: request.rendered_input, output_format: request.output_format }),
    ).byteLength >
      64 * 1024
  ) {
    return {
      code: "unsupported_execution",
      message: "DigitalOcean input exceeds its bounded exec transport",
    };
  }
  return undefined;
}
