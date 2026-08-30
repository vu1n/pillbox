// §0 envelope — reconciled with pillbox/src/contract.rs::Event (the canonical
// Rust contract) + the envelope additions docs/session-event-log.md specs but
// the Rust side hasn't built yet (flagged inline as "spec, not in contract.rs").
// Wire rules match contract.rs exactly: camelCase fields, payload internally
// tagged on `type` with snake_case discriminants, an Unknown catch-all for
// forward-compat.
export interface Event {
  // ── envelope fields that ARE in contract.rs::Event ──
  seq: number; // monotonic per session; 0 is unassigned before trusted local ingestion (Rust: Event::session sets 0).
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

// docs/session-event-log.md §Actor model. Stamped at trusted ingestion from a
// verified identity, never self-reported by the producer — the trust boundary.
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
// NOTE: `input` and `annotation` ARE now in contract.rs::Payload (added with the
// multiplayer/actor work); `driver_changed` remains spec-only (docs/session-event-log.md
// §Payload taxonomy), modeled here because the spike's driver path exercises it.
export type Payload =
  // the durable, attributed steer (matches contract.rs::Input). Always a discrete turn —
  // live keystrokes are the ephemeral Frame::Input, a different path — so no `mode`.
  // `target` mirrors InputTarget. (binary `data` is deferred BOTH sides — not a field yet,
  // so it isn't declared here either; add it to contract.rs::Input first to keep parity.)
  | { type: "input"; text?: string; target: "agent" | "pty" | "exec" }
  // the async, attributed "chime in" that does NOT drive (matches contract.rs::
  // Annotation) — how a non-driver participates; an orchestrator may inject it as
  // agent context. `anchor` references what it's about (a seq, a path, a message id).
  | { type: "annotation"; text: string; anchor?: string }
  // §Multiplayer driver arbitration: who currently holds the single driver slot.
  // Spec'd in session-event-log.md, not (yet) in contract.rs::Payload. `to` is
  // optional because `released` clears the driver (no successor); the *event* actor
  // is the collaboration authority (system), not the new driver.
  | { type: "driver_changed"; from?: Actor; to?: Actor; mode: "granted" | "requested" | "stolen" | "released" }
  // in contract.rs::Payload (field shapes match the Rust structs):
  // The agent-output stream the OpencodeMapper emits (consume path). message_start/
  // thinking/usage were previously absorbed by the catch-all (rust-only); modeled
  // explicitly so the parity gate field-checks them against contract.rs.
  | { type: "message_start"; messageId: string; role: string }
  | { type: "message_delta"; messageId: string; text: string }
  | { type: "message_end"; messageId: string; model?: string; stopReason?: string }
  | { type: "thinking"; text: string }
  | { type: "tool_call"; toolCallId: string; name: string; status: string; input?: unknown; output?: string; title?: string }
  | { type: "usage"; messageId: string; inputTokens?: number; outputTokens?: number; cacheReadInputTokens?: number; cacheCreationInputTokens?: number; costUsd?: number; source: string }
  | { type: "attention_required"; reason: string; message: string }
  // catch-all == contract.rs Payload::Unknown (any unmodeled `type`):
  | { type: string; [k: string]: unknown };
