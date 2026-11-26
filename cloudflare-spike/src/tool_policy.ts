import type { ExecuteInvocationV2Request } from "./codex_execution.js";

/** The tool policy as the execution request carries it. */
export type ToolPolicy = ExecuteInvocationV2Request["tool_policy"];

export type OpencodeConfig = {
  provider?: Record<string, { options?: { apiKey?: string }; models?: Record<string, unknown> }>;
} & Record<string, unknown>;

const OPENCODE_BUILTIN_TOOLS = [
  "bash",
  "read",
  "write",
  "edit",
  "patch",
  "glob",
  "grep",
  "list",
  "task",
  "webfetch",
  "websearch",
  "codesearch",
  "todowrite",
  "todoread",
  "question",
  "skill",
  "lsp",
] as const;

/** Server-side OpenCode permissions for the request's tool policy; only deny_all is enforceable. */
export function enforceOpencodeToolPolicy(
  config: OpencodeConfig | undefined,
  policy: ToolPolicy,
): OpencodeConfig {
  requireDenyAll(policy);
  return { ...(config ?? {}), permission: "deny" };
}

/** Per-prompt tool switches for the request's tool policy: defense in depth for a running server. */
export function opencodePromptTools(policy: ToolPolicy): Record<string, false> {
  requireDenyAll(policy);
  return Object.fromEntries(
    OPENCODE_BUILTIN_TOOLS.map((tool) => [tool, false] as const),
  );
}

// Managed execution enforces only deny_all until credentials are brokered; any other policy fails closed.
function requireDenyAll(policy: ToolPolicy): void {
  if (policy !== "deny_all")
    throw new Error(`unsupported tool policy: ${policy}`);
}
