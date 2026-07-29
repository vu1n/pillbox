// Port of pillbox/src/events/opencode.rs::EventMapper to TypeScript, for the
// managed-tier §0 gateway's **consume path** (docs/managed-tier.md §Consume path).
//
// One instance per session stream. `onEvent` maps a single opencode `/event`
// envelope into zero or more §0 [`Payload`]s, which `SessionGateway` appends —
// stamped with the agent actor (never self-reported by opencode-in-the-box).
//
// Kept 1:1 with the Rust mapper. Two gates guard the "one §0, two backends"
// promise: `check-contract-parity.py` checks the payload *shapes* (contract.ts ↔
// contract.rs); `opencode_mapper.test.ts` checks the mapping *logic* against
// fixtures cribbed from opencode.rs's own tests. Change both sides together.
//
// Faithfulness note: contract.rs omits empty/None fields on the wire
// (`skip_serializing_if`), so this mapper likewise omits `output`/`input`/the
// absent token counts rather than emitting empty strings — so a future
// cross-language fixture diff (the documented sequel to the contract-parity gate)
// can compare serialized payloads byte-for-byte.
//
// The opencode event inputs are typed `any` on purpose: they're untyped wire JSON
// (the analog of opencode.rs's `&serde_json::Value`). The strong contract is the
// `Payload[]` OUTPUT; `unknown` would force a cast at every `?.` access the chains
// already guard, so `any` is the deliberate boundary type here, not lazy typing.

import type { Payload } from "./contract.js";

// opencode status → §0 ToolStatus (snake_case): pending/running → "running";
// "completed"; "error". Mirrors opencode.rs::map_tool_status.
function mapToolStatus(s: string): string {
  switch (s) {
    case "completed":
      return "completed";
    case "error":
      return "error";
    default:
      return "running"; // pending / running / anything mid-flight
  }
}

function attention(reason: string): Payload {
  return { type: "attention_required", reason, message: "" };
}

// session.error → a message (string, or an object with `message` / `data.message`).
// Mirrors opencode.rs::error_message.
function errorMessage(props: any): string {
  const e = props?.error;
  if (typeof e === "string") return e;
  if (e && typeof e === "object") {
    return (e.message ?? e.data?.message ?? "") as string;
  }
  return "";
}

// A finished step's `tokens` → §0 `usage` (source: native), or null when no
// modelled token field is present (a {total}-only step yields nothing, matching
// opencode.rs::usage_from_step). Token fields are omitted when absent.
function usageFromStep(part: any): Payload | null {
  const tokens = part?.tokens;
  if (!tokens) return null;
  const num = (o: any, k: string): number | undefined =>
    typeof o?.[k] === "number" ? o[k] : undefined;
  const cache = tokens.cache ?? {};
  const input = num(tokens, "input");
  const output = num(tokens, "output");
  const cacheRead = num(cache, "read");
  const cacheCreation = num(cache, "write");
  if (
    input === undefined &&
    output === undefined &&
    cacheRead === undefined &&
    cacheCreation === undefined
  ) {
    return null;
  }
  return {
    type: "usage",
    messageId: typeof part.messageID === "string" ? part.messageID : "",
    source: "native",
    ...(input !== undefined ? { inputTokens: input } : {}),
    ...(output !== undefined ? { outputTokens: output } : {}),
    ...(cacheRead !== undefined ? { cacheReadInputTokens: cacheRead } : {}),
    ...(cacheCreation !== undefined ? { cacheCreationInputTokens: cacheCreation } : {}),
  };
}

/** Stateful opencode-event → §0-payload mapper. One per session stream. */
export class OpencodeMapper {
  // The currently-open assistant message id (set on the first `message.updated`
  // for an assistant message, cleared on `session.idle`). Suppresses duplicate
  // MessageStarts without an ever-growing seen-set — opencode opens exactly one
  // assistant message per turn.
  private openMsg: string | null = null;
  // The assistant message whose final schema-bound value has been emitted.
  // OpenCode carries structured output on message.updated rather than text
  // parts, so project it into the same MessageDelta evidence channel once.
  private structuredMsg: string | null = null;
  // callID → last emitted (mapped) tool status, so a ToolCall is emitted only on
  // a status transition, not on every input-stream tick.
  private toolStatus = new Map<string, string>();
  // step part ids whose step-finish usage we've already emitted, so a re-sent
  // part.updated for the same step can't double-count tokens.
  private stepsSeen = new Set<string>();

  /** Map one opencode `/event` envelope into zero or more §0 payloads. */
  onEvent(ev: any): Payload[] {
    const ty: string = ev?.type ?? "";
    const p = ev?.properties ?? {};
    switch (ty) {
      case "message.updated":
        return this.onMessageUpdated(p);
      case "message.part.delta":
        return this.onPartDelta(p);
      case "message.part.updated":
        return this.onPartUpdated(p);
      // Turn went quiescent → close the open assistant message and raise the
      // attention signal the driver waits on.
      case "session.idle": {
        const out: Payload[] = [];
        if (this.openMsg !== null) {
          out.push({ type: "message_end", messageId: this.openMsg });
          this.openMsg = null;
        }
        this.structuredMsg = null;
        out.push(attention("needs_input"));
        return out;
      }
      case "permission.asked":
        return [attention("permission")];
      case "question.asked":
        return [attention("needs_input")];
      case "session.error":
        return [{ type: "attention_required", reason: "error_stalled", message: errorMessage(p) }];
      default:
        return [];
    }
  }

  // `message.updated` — open an assistant message on its first sighting and
  // project OpenCode's final schema-bound value into the text evidence channel.
  // User messages and repeats without new structured output produce nothing.
  private onMessageUpdated(p: any): Payload[] {
    const info = p?.info ?? {};
    const role: string = info.role ?? "";
    const id: string = info.id ?? "";
    if (role !== "assistant" || id === "") return [];
    const out: Payload[] = [];
    if (this.openMsg !== id) {
      this.openMsg = id;
      out.push({ type: "message_start", messageId: id, role: "assistant" });
    }
    if (info.structured !== undefined && this.structuredMsg !== id) {
      this.structuredMsg = id;
      out.push({
        type: "message_delta",
        messageId: id,
        text: JSON.stringify(info.structured),
      });
    }
    return out;
  }

  // `message.part.delta` — streaming content. `field` selects the §0 channel:
  // assistant text vs. reasoning/thinking. Empty deltas drop.
  private onPartDelta(p: any): Payload[] {
    const delta: string = p?.delta ?? "";
    if (delta === "") return [];
    const field: string = p?.field ?? "text";
    if (field === "reasoning") return [{ type: "thinking", text: delta }];
    // Attach to the delta's own messageID, falling back to the open assistant
    // message. Neither → nothing to attach to, drop it.
    const messageId: string | null =
      (typeof p?.messageID === "string" ? p.messageID : null) ?? this.openMsg;
    if (messageId === null) return [];
    return [{ type: "message_delta", messageId, text: delta }];
  }

  // `message.part.updated` — `tool` (evolving tool state) and `step-finish`
  // (token accounting) carry the turn; other parts produce nothing.
  private onPartUpdated(p: any): Payload[] {
    const part = p?.part ?? {};
    switch (part?.type) {
      case "tool":
        return this.onToolPart(part);
      case "step-finish":
        return this.onStepFinish(part);
      default:
        return [];
    }
  }

  // A `tool` part. Emits a ToolCall only when the mapped status changes.
  private onToolPart(part: any): Payload[] {
    const callId: string = part?.callID ?? "";
    const state = part?.state ?? {};
    const status = mapToolStatus(state?.status ?? "running");
    if (this.toolStatus.get(callId) === status) return [];
    this.toolStatus.set(callId, status);
    const output = typeof state?.output === "string" ? state.output : "";
    return [
      {
        type: "tool_call",
        toolCallId: callId,
        name: part?.tool ?? "",
        status,
        // Omit empty/absent fields to match contract.rs's skip_serializing_if.
        // The two checks differ deliberately — don't normalize: `input` mirrors
        // `Option<Value>` (omit when null), `output` mirrors `String` (omit when "").
        ...(state?.input != null ? { input: state.input } : {}),
        ...(output !== "" ? { output } : {}),
      },
    ];
  }

  // A finished model step's token usage — one §0 Usage per step, de-duped on the
  // step part id so a re-sent part.updated doesn't double-count.
  private onStepFinish(part: any): Payload[] {
    const usage = usageFromStep(part);
    if (usage === null) return [];
    const id = part?.id;
    if (typeof id === "string") {
      if (this.stepsSeen.has(id)) return [];
      this.stepsSeen.add(id);
    }
    return [usage];
  }
}
