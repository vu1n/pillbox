// §0 envelope — 1:1 with pillbox/src/contract.rs::Event.
// camelCase fields; payload internally tagged on `type` (snake_case).
export interface Event {
  v?: number; // schema version (per-line; managed-tier addition)
  seq: number; // monotonic per SESSION, gateway-assigned. 0 = unassigned on submit.
  sessionId: string; // partition key
  at: string; // RFC3339
  ephemeral?: boolean; // seq 0 / excluded from replay (contract.rs parity)
  actor?: Actor; // managed-tier field; STUB here (not yet authenticated)
  // optional correlation (contract.rs: demoted to optional)
  sandboxId?: string;
  runId?: string;
  execId?: string;
  payload: Payload;
}

export interface Actor {
  kind: "human" | "agent" | "system" | "service";
  id: string;
  display?: string;
}

// Payload mirrors contract.rs::Payload. The spike exercises `input` (the
// attributed steer) + a couple of agent-output variants; any other `type`
// is accepted verbatim (forward-compat, == Payload::Unknown).
export type Payload =
  | { type: "input"; text?: string; data?: string; target: "agent" | "pty" | "exec"; mode: "live" | "turn" }
  | { type: "message_delta"; messageId: string; text: string }
  | { type: "message_end"; messageId: string; model?: string; stopReason?: string }
  | { type: "tool_call"; toolCallId: string; name: string; status: string; input?: unknown; output?: string; title?: string }
  | { type: "attention_required"; reason: string; message: string }
  | { type: string; [k: string]: unknown }; // Unknown catch-all
