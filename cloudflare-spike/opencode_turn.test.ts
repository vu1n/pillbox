import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import test from "node:test";
import type { Payload } from "./src/contract.ts";
import type { OpencodeTurnSink } from "./src/opencode_turn.ts";

// Production source uses emitted `.js` specifiers. Node's type-stripping test
// runner executes the `.ts` sources directly, so map only local source imports.
registerHooks({
  resolve(specifier, context, nextResolve) {
    return nextResolve(
      context.parentURL?.includes("/cloudflare-spike/src/") &&
        specifier.startsWith(".") &&
        specifier.endsWith(".js")
        ? `${specifier.slice(0, -3)}.ts`
        : specifier,
      context,
    );
  },
});

const { driveOpencodeTurn, readStreamChunkWithTimeout } = await import(
  "./src/opencode_turn.ts"
);

const outputFormat = {
  type: "json_schema" as const,
  retry_count: 2 as const,
  schema: {
    type: "object",
    additionalProperties: false,
    required: ["kind", "text"],
    properties: {
      kind: { const: "document" },
      text: { type: "string" },
    },
  },
};

test("OpenCode turn returns exact structured output through the evidence sink", async () => {
  const transport = fakeTransport([
    sse(
      event("message.updated", {
        info: {
          role: "assistant",
          id: "msg:1",
          structured: { kind: "document", text: "# Grill" },
        },
      }),
      event("session.idle", {}),
    ),
  ]);
  const evidence = captureEvidence();

  const output = await driveOpencodeTurn({
    sandbox: transport.sandbox,
    text: "Produce the document.",
    model: "zai-coding-plan/glm-4.7",
    outputFormat,
    config: { env: {} },
    sink: evidence.sink,
  });

  assert.equal(output, '{"kind":"document","text":"# Grill"}');
  assert.deepEqual(transport.promptPaths, ["/session/session:1/prompt_async"]);
  assert.deepEqual(evidence.errors, []);
  assert.equal(
    evidence.agent.some(
      (payload) =>
        payload.type === "message_delta" &&
        payload.text === '{"kind":"document","text":"# Grill"}',
    ),
    true,
  );
});

test("OpenCode turn retries in a fresh session and records raw JSON acceptance", async () => {
  const transport = fakeTransport([
    sse(
      event("message.part.delta", {
        messageID: "msg:1",
        partID: "part:1",
        field: "text",
        delta: "not JSON",
      }),
      event("session.idle", {}),
    ),
    sse(
      event("message.part.delta", {
        messageID: "msg:2",
        partID: "part:2",
        field: "text",
        delta: '{"kind":"document","text":"# Recovered"}',
      }),
      event("session.idle", {}),
    ),
  ]);
  const evidence = captureEvidence();
  const warnings: unknown[][] = [];
  const originalWarn = console.warn;
  console.warn = (...args: unknown[]) => void warnings.push(args);

  let output: string | undefined;
  try {
    output = await driveOpencodeTurn({
      sandbox: transport.sandbox,
      text: "Produce the document.",
      model: "zai-coding-plan/glm-4.7",
      outputFormat,
      config: { env: {} },
      sink: evidence.sink,
    });
  } finally {
    console.warn = originalWarn;
  }

  assert.equal(output, '{"kind":"document","text":"# Recovered"}');
  assert.deepEqual(transport.promptPaths, [
    "/session/session:1/prompt_async",
    "/session/session:2/prompt_async",
  ]);
  assert.match(
    transport.promptBodies[1].parts[0].text,
    /Structured-output retry 1 of 2/,
  );
  assert.deepEqual(
    evidence.system.map(({ name }) => name),
    ["structured_output.retry", "structured_output.raw_json_fallback"],
  );
  assert.deepEqual(warnings, [
    [
      "raw structured output rejected",
      "invalid_json",
      "assistant text contains no schema-valid JSON value",
    ],
  ]);
  assert.deepEqual(evidence.errors, []);
});

test("OpenCode turn rejects an oversized session response before JSON parsing", async () => {
  const transport = fakeTransport([sse(event("session.idle", {}))], {
    sessionResponse: () =>
      new Response(`{"id":"${"a".repeat(256 * 1024)}"}`, {
        headers: { "content-type": "application/json" },
      }),
  });
  const evidence = captureEvidence();

  const output = await driveOpencodeTurn({
    sandbox: transport.sandbox,
    text: "hello",
    model: "zai-coding-plan/glm-4.7",
    config: { env: {} },
    sink: evidence.sink,
  });

  assert.equal(output, undefined);
  assert.match(evidence.errors[0] ?? "", /response exceeded 262144 bytes/);
});

test("OpenCode turn rejects an oversized SSE frame before accumulation", async () => {
  const transport = fakeTransport([`data: ${"x".repeat(256 * 1024 + 1)}\n\n`]);
  const evidence = captureEvidence();

  const output = await driveOpencodeTurn({
    sandbox: transport.sandbox,
    text: "hello",
    model: "zai-coding-plan/glm-4.7",
    config: { env: {} },
    sink: evidence.sink,
  });

  assert.equal(output, undefined);
  assert.match(evidence.errors[0] ?? "", /event stream frame exceeded 262144 bytes/);
});

test("OpenCode SSE idle timeout directly cancels its pending reader", async () => {
  let cancelled = false;
  const stream = new ReadableStream<Uint8Array>({
    cancel: () => {
      cancelled = true;
    },
  });
  const reader = stream.getReader();

  await assert.rejects(
    readStreamChunkWithTimeout(reader, 5),
    /no event-stream bytes for 5ms/,
  );
  assert.equal(cancelled, true);
});

function captureEvidence(): {
  readonly agent: Payload[];
  readonly errors: string[];
  readonly system: Array<{
    readonly idPrefix: string;
    readonly name: string;
    readonly input?: Record<string, unknown>;
    readonly output: string;
  }>;
  readonly sink: OpencodeTurnSink;
} {
  const agent: Payload[] = [];
  const errors: string[] = [];
  const system: Array<{
    readonly idPrefix: string;
    readonly name: string;
    readonly input?: Record<string, unknown>;
    readonly output: string;
  }> = [];
  return {
    agent,
    errors,
    system,
    sink: {
      appendAgent: (payload) => void agent.push(payload),
      appendError: (message) => void errors.push(message),
      appendSystemTool: (input) => void system.push(input),
    },
  };
}

function fakeTransport(
  eventStreams: readonly string[],
  options: { readonly sessionResponse?: () => Response } = {},
): {
  readonly sandbox: never;
  readonly promptPaths: string[];
  readonly promptBodies: Array<{
    readonly parts: ReadonlyArray<{ readonly text: string }>;
  }>;
} {
  let streamIndex = 0;
  let sessionIndex = 0;
  const promptPaths: string[] = [];
  const promptBodies: Array<{
    readonly parts: ReadonlyArray<{ readonly text: string }>;
  }> = [];
  const sandbox = {
    containerFetch: async (request: Request): Promise<Response> => {
      const path = new URL(request.url).pathname;
      if (request.method === "GET" && path === "/doc") {
        return new Response("ok");
      }
      if (request.method === "GET" && path === "/event") {
        const body = eventStreams[streamIndex++];
        assert.notEqual(body, undefined, "unexpected OpenCode event stream");
        return new Response(body, {
          headers: { "content-type": "text/event-stream" },
        });
      }
      if (request.method === "POST" && path === "/session") {
        sessionIndex += 1;
        if (options.sessionResponse) return options.sessionResponse();
        return Response.json({ id: `session:${sessionIndex}` });
      }
      if (request.method === "POST" && path.endsWith("/prompt_async")) {
        promptPaths.push(path);
        promptBodies.push(
          (await request.json()) as {
            readonly parts: ReadonlyArray<{ readonly text: string }>;
          },
        );
        return new Response(null, { status: 204 });
      }
      return new Response("not found", { status: 404 });
    },
    killAllProcesses: async () => {},
    startProcess: async () => {},
  };
  return {
    sandbox: sandbox as never,
    promptPaths,
    promptBodies,
  };
}

function event(type: string, properties: unknown): unknown {
  return { type, properties };
}

function sse(...events: readonly unknown[]): string {
  return events.map((value) => `data: ${JSON.stringify(value)}\n\n`).join("");
}
