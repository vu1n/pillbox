export type HuddlesToolPolicy = "deny_all";

export type HuddlesOpencodeConfig = {
  provider?: Record<string, { options?: { apiKey?: string } }>;
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

/** Names in this namespace are reserved for private Huddles service-binding sessions. */
export function isHuddlesSessionName(value: unknown): value is string {
  return typeof value === "string" && /^ensure-[0-9a-f]{64}$/.test(value);
}

/** Authoritative server policy for Huddles-managed OpenCode sessions. */
export function enforceHuddlesOpencodePolicy(
  config: HuddlesOpencodeConfig | undefined,
  policy: HuddlesToolPolicy,
): HuddlesOpencodeConfig {
  if (policy !== "deny_all")
    throw new Error(`unsupported tool policy: ${policy}`);
  return { ...(config ?? {}), permission: "deny" };
}

/** Defense in depth for an already-running or unexpectedly configured server. */
export function huddlesPromptTools(
  policy: HuddlesToolPolicy,
): Record<string, false> {
  if (policy !== "deny_all")
    throw new Error(`unsupported tool policy: ${policy}`);
  return Object.fromEntries(
    OPENCODE_BUILTIN_TOOLS.map((tool) => [tool, false] as const),
  );
}

export function classifyRunningInvocation(
  ownedByCurrentIsolate: boolean,
): "running" | "interrupted" {
  return ownedByCurrentIsolate ? "running" : "interrupted";
}
