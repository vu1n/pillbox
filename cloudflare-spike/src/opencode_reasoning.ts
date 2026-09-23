import type { RequestedModelProfile } from "./codex_execution.js";

/** Verified against OpenRouter's model metadata and pinned OpenCode's OpenRouter variant mapping. */
export function openRouterReasoning(requested: RequestedModelProfile) {
  if (
    requested.provider !== "openrouter" ||
    requested.model !== "openai/gpt-5-mini" ||
    !["low", "medium", "high"].includes(requested.reasoning_effort)
  )
    return undefined;
  return {
    variant: `pillbox-${requested.reasoning_effort}`,
    options: {
      reasoning: { effort: requested.reasoning_effort },
      // A route that drops the sealed reasoning control must fail, not sample with defaults.
      provider: { require_parameters: true },
    },
  };
}

export function openRouterReasoningAdmission(requested: RequestedModelProfile) {
  if (requested.provider === "openrouter" && !openRouterReasoning(requested)) {
    return {
      code: "unsupported_execution" as const,
      message:
        "OpenRouter managed execution requires GPT-5 mini with low, medium, or high reasoning effort",
    };
  }
  return undefined;
}
