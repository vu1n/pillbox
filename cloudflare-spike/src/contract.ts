// §0 envelope — reconciled with pillbox/src/contract.rs::Event (the canonical
// Rust contract) + the envelope additions docs/session-event-log.md specs but
// the Rust side hasn't built yet (flagged inline as "spec, not in contract.rs").
// Wire rules match contract.rs exactly: camelCase fields, payload internally
// tagged on `type` with snake_case discriminants, an Unknown catch-all for
// forward-compat.
export interface Event {
  // ── envelope fields that ARE in contract.rs::Event ──
  seq: number; // monotonic per SESSION, gateway-assigned. 0 = unassigned on submit (Rust: Event::session sets 0).
  sessionId: string; // partition key (contract.rs: the durable identity)
  at: string; // RFC3339
  ephemeral?: boolean; // contract.rs: `ephemeral` bool, default false / omitted; seq 0, excluded from replay
  // optional correlation — contract.rs demotes sandbox/run/exec to optional
  sandboxId?: string;
  runId?: string;
  execId?: string;
  actor?: Actor; // who produced this event — stamped from a verified token (auth.ts), never the body. In contract.rs::Event.
  payload: Payload;

  // ── envelope fields spec'd in docs/session-event-log.md but NOT yet in
  // contract.rs::Event (additive; safe to carry here ahead of the Rust side) ──
  v?: number; // schema version (per-line). Spec'd in session-event-log.md §Envelope; absent from contract.rs.
  causationId?: number; // seq of the event that caused this. Spec'd; not in contract.rs.
  class?: "content" | "signal"; // poolability split. Spec'd as `class`; not in contract.rs.
  idempotencyKey?: string; // per-event append dedup on retry. Spec'd; not in contract.rs (only on RPCs).
}

// docs/session-event-log.md §Actor model. Stamped by the gateway from a verified
// token (src/auth.ts), never self-reported by the producer — the trust boundary.
// Mirrors contract.rs::Actor.
export interface Actor {
  kind: "human" | "agent" | "system" | "service";
  id: string;
  display?: string;
}

// Payload mirrors contract.rs::Payload — same `type` discriminants (snake_case),
// same camelCase fields. The spike models the variants it exercises; every other
// `type` is accepted verbatim (forward-compat, == contract.rs Payload::Unknown).
//
// NOTE: `input` is NOT in contract.rs::Payload — it's a multiplayer payload
// spec'd in docs/session-event-log.md (§Payload taxonomy, the durable attributed
// steer). Kept because the spike's /input path exercises it; flagged as spec.
export type Payload =
  // spec'd (session-event-log.md), not in contract.rs::Payload:
  | { type: "input"; text?: string; data?: string; target: "agent" | "pty" | "exec"; mode: "live" | "turn" }
  // in contract.rs::Payload (field shapes match the Rust structs):
  | { type: "message_delta"; messageId: string; text: string }
  | { type: "message_end"; messageId: string; model?: string; stopReason?: string }
  | { type: "tool_call"; toolCallId: string; name: string; status: string; input?: unknown; output?: string; title?: string }
  | { type: "attention_required"; reason: string; message: string }
  // catch-all == contract.rs Payload::Unknown (any unmodeled `type`):
  | { type: string; [k: string]: unknown };
