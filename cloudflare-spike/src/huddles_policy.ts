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

/** A fresh-session correction after OpenCode idles without its schema tool. */
export function structuredOutputRetryPrompt(
  originalPrompt: string,
  retryNumber: number,
  retryCount: number,
): string {
  if (
    !Number.isInteger(retryNumber) ||
    retryNumber < 1 ||
    !Number.isInteger(retryCount) ||
    retryNumber > retryCount
  ) {
    throw new Error("invalid structured-output retry ordinal");
  }
  return `${originalPrompt}

Structured-output retry ${retryNumber} of ${retryCount}: the previous attempt ended without calling StructuredOutput. Call StructuredOutput exactly once with a JSON value that matches the supplied schema. If this runtime cannot expose StructuredOutput, return exactly one bare JSON value matching that schema, with no prose or Markdown fence.`;
}

/** Operator-safe detail for durable §0 evidence and Worker logs. */
export function safeHuddlesRuntimeDiagnostic(cause: unknown): string {
  const raw =
    cause instanceof Error
      ? `${cause.name}: ${cause.message}`
      : typeof cause === "string"
        ? cause
        : "unknown runtime error";
  return (
    raw
      .replace(/https?:\/\/\S+/gi, "[url redacted]")
      .replace(/\b(?:cfat_|sk-)[A-Za-z0-9_-]+/gi, "[credential redacted]")
      .replace(/[A-Za-z0-9_-]{32,}/g, "[opaque value redacted]")
      .replace(/\s+/g, " ")
      .trim()
      .slice(0, 240) || "unknown runtime error"
  );
}
