// Mapper logic gate — a 1:1 port of pillbox/src/events/opencode.rs's mapper tests.
// Same opencode `/event` envelopes in, same §0 payloads out, so the two
// implementations of the mapping can't drift (the cross-language fixture check
// docs/managed-tier.md §Consume path calls for). Run: `node --test opencode_mapper.test.ts`
// (Node ≥ 22.6 strips the TS types; the mapper's only import is `import type`, erased).

import { test } from "node:test";
import assert from "node:assert/strict";
import { OpencodeMapper } from "./src/opencode_mapper.ts";

// opencode wire envelope: { type, properties }. Mirrors opencode.rs's `ev()` helper.
const ev = (type: string, properties: unknown) => ({ type, properties });

test("the user-message echo of our own prompt maps to nothing", () => {
  const m = new OpencodeMapper();
  assert.deepEqual(
    m.onEvent(ev("message.updated", { sessionID: "ses_a", info: { id: "msg_u", role: "user" } })),
    [],
  );
});

test("assistant turn: start (once) → text deltas → idle ends + raises NeedsInput", () => {
  const m = new OpencodeMapper();

  // First sight of the assistant message → message_start.
  assert.deepEqual(
    m.onEvent(ev("message.updated", { sessionID: "ses_a", info: { id: "msg_a", role: "assistant" } })),
    [{ type: "message_start", messageId: "msg_a", role: "assistant" }],
  );
  // A repeat update of the same message does NOT re-open it.
  assert.deepEqual(
    m.onEvent(ev("message.updated", { sessionID: "ses_a", info: { id: "msg_a", role: "assistant" } })),
    [],
  );
  // Streaming text delta carries the messageID.
  assert.deepEqual(
    m.onEvent(ev("message.part.delta", { messageID: "msg_a", partID: "prt_1", field: "text", delta: "hi" })),
    [{ type: "message_delta", messageId: "msg_a", text: "hi" }],
  );
  // Empty deltas drop.
  assert.deepEqual(
    m.onEvent(ev("message.part.delta", { messageID: "msg_a", field: "text", delta: "" })),
    [],
  );
  // Idle ends the open message and raises NeedsInput.
  assert.deepEqual(m.onEvent(ev("session.idle", { sessionID: "ses_a" })), [
    { type: "message_end", messageId: "msg_a" },
    { type: "attention_required", reason: "needs_input", message: "" },
  ]);
  assert.equal(m.mayRetryStructuredOutput(), true);
  assert.equal(m.plainTextOutput(), "hi");
});

test("schema-bound assistant output maps once into the message evidence channel", () => {
  const m = new OpencodeMapper();
  const structured = {
    kind: "document",
    text: "# Grill\n\nChallenge the assumptions.",
  };
  assert.deepEqual(
    m.onEvent(
      ev("message.part.delta", {
        messageID: "msg_a",
        field: "text",
        delta: "provider retry preamble",
      }),
    ),
    [{ type: "message_delta", messageId: "msg_a", text: "provider retry preamble" }],
  );
  assert.deepEqual(
    m.onEvent(
      ev("message.updated", {
        sessionID: "ses_a",
        info: { id: "msg_a", role: "assistant", structured },
      }),
    ),
    [
      { type: "message_start", messageId: "msg_a", role: "assistant" },
      {
        type: "message_delta",
        messageId: "msg_a",
        text: JSON.stringify(structured),
      },
    ],
  );
  assert.equal(m.structuredOutput(), JSON.stringify(structured));
  assert.equal(m.mayRetryStructuredOutput(), false);
  assert.equal(m.plainTextOutput(), "provider retry preamble");
  assert.deepEqual(
    m.onEvent(
      ev("message.updated", {
        sessionID: "ses_a",
        info: { id: "msg_a", role: "assistant", structured },
      }),
    ),
    [],
  );
  assert.equal(m.structuredOutput(), JSON.stringify(structured));
});

test("raw output selects the final text part without dropping preamble evidence", () => {
  const m = new OpencodeMapper();
  assert.deepEqual(
    m.onEvent(
      ev("message.part.delta", {
        messageID: "msg_a",
        partID: "prt_preamble",
        field: "text",
        delta: "I will now provide the answer.",
      }),
    ),
    [
      {
        type: "message_delta",
        messageId: "msg_a",
        text: "I will now provide the answer.",
      },
    ],
  );
  m.onEvent(
    ev("message.part.delta", {
      messageID: "msg_a",
      partID: "prt_answer",
      field: "text",
      delta: '{"kind":"doc',
    }),
  );
  m.onEvent(
    ev("message.part.delta", {
      messageID: "msg_a",
      partID: "prt_answer",
      field: "text",
      delta: 'ument","text":"# Grill"}',
    }),
  );
  assert.equal(
    m.plainTextOutput(),
    '{"kind":"document","text":"# Grill"}',
  );
});

test("reasoning delta maps to thinking", () => {
  const m = new OpencodeMapper();
  assert.deepEqual(
    m.onEvent(ev("message.part.delta", { messageID: "m", field: "reasoning", delta: "hmm" })),
    [{ type: "thinking", text: "hmm" }],
  );
});

test("tool part emits on status change only (dedup + omit empty fields)", () => {
  const m = new OpencodeMapper();
  const tool = (status: string, state: object) =>
    ev("message.part.updated", { part: { id: "prt_t", messageID: "m", type: "tool", tool: "ls", callID: "call_1", state: { status, ...state } } });

  // pending → running (with name + input; output omitted because absent).
  assert.deepEqual(m.onEvent(tool("pending", { input: { path: "." } })), [
    { type: "tool_call", toolCallId: "call_1", name: "ls", status: "running", input: { path: "." } },
  ]);
  // running → still "running" after mapping → no duplicate.
  assert.deepEqual(m.onEvent(tool("running", { input: { path: "." } })), []);
  // completed → completed with output (input omitted because absent).
  assert.deepEqual(
    m.onEvent(ev("message.part.updated", { part: { type: "tool", tool: "ls", callID: "call_1", state: { status: "completed", output: "a\nb" } } })),
    [{ type: "tool_call", toolCallId: "call_1", name: "ls", status: "completed", output: "a\nb" }],
  );
});

test("step-finish emits one native usage, de-duped on step id", () => {
  const m = new OpencodeMapper();
  const step = ev("message.part.updated", {
    part: { id: "prt_step", messageID: "msg_a", type: "step-finish", tokens: { input: 120, output: 30, reasoning: 5, cache: { read: 100, write: 20 } } },
  });
  assert.deepEqual(m.onEvent(step), [
    { type: "usage", messageId: "msg_a", source: "native", inputTokens: 120, outputTokens: 30, cacheReadInputTokens: 100, cacheCreationInputTokens: 20 },
  ]);
  // A re-sent part.updated for the same step id must not double-count.
  assert.deepEqual(m.onEvent(step), []);
});

test("step-finish without modelled tokens is ignored", () => {
  const m = new OpencodeMapper();
  assert.deepEqual(
    m.onEvent(ev("message.part.updated", { part: { id: "prt_s", type: "step-finish", tokens: { total: 10 } } })),
    [],
  );
});

test("step-finish preserves provider-reported cost", () => {
  const mapper = new OpencodeMapper();
  assert.deepEqual(
    mapper.onEvent(
      ev("message.part.updated", {
        part: {
          id: "prt_cost",
          messageID: "msg_cost",
          type: "step-finish",
          cost: 0.0125,
          tokens: { input: 10, output: 2 },
        },
      }),
    ),
    [
      {
        type: "usage",
        messageId: "msg_cost",
        source: "native",
        inputTokens: 10,
        outputTokens: 2,
        costUsd: 0.0125,
      },
    ],
  );
});

test("snapshots, lifecycle, session.next.* and server.* are ignored", () => {
  const m = new OpencodeMapper();
  for (const e of [
    ev("message.part.updated", { part: { type: "text", text: "full text so far" } }),
    ev("message.part.updated", { part: { type: "step-finish", tokens: { total: 10 } } }),
    ev("session.next.text.delta", { sessionID: "s", delta: "x" }),
    ev("session.next.model.switched", { sessionID: "s" }),
    ev("session.updated", { sessionID: "s" }),
    ev("server.heartbeat", {}),
  ]) {
    assert.deepEqual(m.onEvent(e), [], `should ignore: ${e.type}`);
  }
});

test("session.error raises ErrorStalled with the extracted message", () => {
  const m = new OpencodeMapper();
  assert.deepEqual(
    m.onEvent(ev("session.error", { error: { message: "boom" } })),
    [{ type: "attention_required", reason: "error_stalled", message: "boom" }],
  );
  assert.equal(m.mayRetryStructuredOutput(), false);
});

test("permission and question stops are not structured-output retry signals", () => {
  const permission = new OpencodeMapper();
  assert.deepEqual(permission.onEvent(ev("permission.asked", {})), [
    { type: "attention_required", reason: "permission", message: "" },
  ]);
  assert.equal(permission.mayRetryStructuredOutput(), false);

  const question = new OpencodeMapper();
  assert.deepEqual(question.onEvent(ev("question.asked", {})), [
    { type: "attention_required", reason: "needs_input", message: "" },
  ]);
  assert.equal(question.mayRetryStructuredOutput(), false);
});
