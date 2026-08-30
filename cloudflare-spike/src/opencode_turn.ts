import type { getSandbox } from "@cloudflare/sandbox";
import type { Payload } from "./contract.js";
import {
  huddlesPromptTools,
  safeHuddlesRuntimeDiagnostic,
  structuredOutputRetryPrompt,
  type HuddlesToolPolicy,
} from "./huddles_policy.js";
import type { JsonSchemaOutputFormat } from "./codex_execution.js";
import { OpencodeMapper } from "./opencode_mapper.js";
import { inspectRawStructuredOutput } from "./structured_output.js";

export const OPENCODE_WORKSPACE_DIR = "/workspace";
export const DEFAULT_OPENCODE_MODEL = "zai-coding-plan/glm-4.5-air";

const OPENCODE_PORT = 4096;
const MAX_TURN_EVENTS = 2_000;
const TURN_TIMEOUT_MS = 300_000;
const EVENT_OPEN_TIMEOUT_MS = 20_000;
const REQUEST_TIMEOUT_MS = 30_000;
const EVENT_IDLE_TIMEOUT_MS = 45_000;

type SandboxHandle = ReturnType<typeof getSandbox>;

export interface OpencodeTurnSink {
  readonly appendAgent: (payload: Payload) => void;
  readonly appendError: (message: string) => void;
  readonly appendSystemTool: (input: {
    readonly idPrefix: string;
    readonly name: string;
    readonly input?: Record<string, unknown>;
    readonly output: string;
  }) => void;
}

export interface OpencodeTurnInput {
  readonly sandbox: SandboxHandle;
  readonly text: string;
  readonly model: string;
  readonly toolPolicy?: HuddlesToolPolicy;
  readonly outputFormat?: JsonSchemaOutputFormat;
  readonly config: {
    readonly config?: unknown;
    readonly env: Readonly<Record<string, string>>;
  };
  readonly sink: OpencodeTurnSink;
}

/**
 * Own one bounded OpenCode turn from container boot through terminal evidence.
 * Durable sequencing and actor attribution remain in the caller-provided sink.
 */
export async function driveOpencodeTurn(
  input: OpencodeTurnInput,
): Promise<string | undefined> {
  const [provider, modelId] = splitModel(input.model);
  if (!modelId) {
    input.sink.appendError(
      `model must be 'provider/modelID' (got '${input.model}')`,
    );
    return;
  }

  const ready = await ensureOpencodeReady(input);
  if (!ready) return;

  const deadline = Date.now() + TURN_TIMEOUT_MS;
  const retryCount = input.outputFormat?.retry_count ?? 0;
  let totalAppended = 0;
  for (let attempt = 0; attempt <= retryCount; attempt++) {
    if (Date.now() > deadline || totalAppended >= MAX_TURN_EVENTS) return;
    const attemptText =
      attempt === 0
        ? input.text
        : structuredOutputRetryPrompt(input.text, attempt, retryCount);
    const result = await driveOpencodeAttempt({
      ...input,
      text: attemptText,
      provider,
      modelId,
      deadline,
      eventBudget: MAX_TURN_EVENTS - totalAppended,
    });
    totalAppended += result.appended;
    if (result.structuredOutput !== undefined) return result.structuredOutput;

    const rawStructuredOutput =
      input.outputFormat && result.mayRetryStructuredOutput
        ? inspectRawStructuredOutput(
            result.plainTextOutput,
            input.outputFormat.schema,
          )
        : undefined;
    if (rawStructuredOutput?.status === "accepted") {
      input.sink.appendSystemTool({
        idPrefix: "structured-output-raw-json",
        name: "structured_output.raw_json_fallback",
        output: "schema-validated JSON fallback accepted",
      });
      return rawStructuredOutput.output;
    }
    if (rawStructuredOutput?.status === "rejected") {
      console.warn(
        "raw structured output rejected",
        rawStructuredOutput.reason,
        rawStructuredOutput.detail,
      );
    }
    if (
      !result.mayRetryStructuredOutput ||
      attempt >= retryCount ||
      Date.now() > deadline ||
      totalAppended >= MAX_TURN_EVENTS
    ) {
      return;
    }
    input.sink.appendSystemTool({
      idPrefix: "structured-output-retry",
      name: "structured_output.retry",
      input: { retry: attempt + 1, retryCount },
      output: "fresh OpenCode session scheduled after schema tool was omitted",
    });
    totalAppended += 1;
  }
}

async function ensureOpencodeReady(input: OpencodeTurnInput): Promise<boolean> {
  let probe = await probeDoc(input.sandbox);
  if (input.toolPolicy && probe.ok) {
    try {
      await input.sandbox.killAllProcesses();
    } catch {
      input.sink.appendError(
        "could not replace an existing OpenCode server with the sealed tool policy",
      );
      return false;
    }
    for (let attempt = 0; attempt < 20 && probe.ok; attempt++) {
      await delay(250);
      probe = await probeDoc(input.sandbox);
    }
    if (probe.ok) {
      input.sink.appendError(
        "existing OpenCode server remained reachable after policy reset",
      );
      return false;
    }
  }
  if (probe.ok) return true;

  const env: Record<string, string> = { ...input.config.env };
  if (input.config.config !== undefined) {
    env.OPENCODE_CONFIG_CONTENT = JSON.stringify(input.config.config);
  }
  let startError = "";
  let started = false;
  for (let attempt = 0; attempt < 60; attempt++) {
    try {
      await input.sandbox.startProcess(
        `cd ${OPENCODE_WORKSPACE_DIR} && opencode serve --port ${OPENCODE_PORT} --hostname 0.0.0.0`,
        { env: Object.keys(env).length > 0 ? env : undefined },
      );
      started = true;
      break;
    } catch (cause) {
      startError = String(cause);
      if (!/starting|not ready/i.test(startError)) break;
      await delay(1_000);
    }
  }
  if (!started) {
    input.sink.appendError(`opencode startProcess failed: ${startError}`);
    return false;
  }
  for (let attempt = 0; attempt < 20 && !probe.ok; attempt++) {
    await delay(1_000);
    probe = await probeDoc(input.sandbox);
  }
  if (!probe.ok) {
    input.sink.appendError(
      `opencode not ready after boot (last probe: ${probe.detail})`,
    );
    return false;
  }
  return true;
}

interface OpencodeAttemptInput extends OpencodeTurnInput {
  readonly provider: string;
  readonly modelId: string;
  readonly deadline: number;
  readonly eventBudget: number;
}

interface OpencodeAttemptResult {
  readonly structuredOutput?: string;
  readonly plainTextOutput?: string;
  readonly mayRetryStructuredOutput: boolean;
  readonly appended: number;
}

/** Drive one fresh OpenCode session within the invocation's shared bounds. */
async function driveOpencodeAttempt(
  input: OpencodeAttemptInput,
): Promise<OpencodeAttemptResult> {
  const failed = (appended = 0): OpencodeAttemptResult => ({
    mayRetryStructuredOutput: false,
    appended,
  });
  const eventResponse = await withTimeoutValue(
    opencodeFetch(input.sandbox, "GET", "/event", undefined, {
      accept: "text/event-stream",
    }),
    EVENT_OPEN_TIMEOUT_MS,
    null,
  );
  if (!eventResponse) {
    input.sink.appendError(
      `opencode /event open timed out (${EVENT_OPEN_TIMEOUT_MS / 1_000}s)`,
    );
    return failed();
  }
  if (!eventResponse.ok || !eventResponse.body) {
    input.sink.appendError(
      `opencode /event stream failed (HTTP ${eventResponse.status})`,
    );
    return failed();
  }

  let sessionId: string;
  try {
    const created = await opencodeFetchWithTimeout(
      input.sandbox,
      "POST",
      "/session",
      {},
      REQUEST_TIMEOUT_MS,
    );
    if (!created.ok) {
      input.sink.appendError(
        `opencode create session failed (HTTP ${created.status})`,
      );
      await cancelBody(eventResponse.body);
      return failed();
    }
    const id = ((await created.json()) as { id?: string }).id;
    if (!id) {
      input.sink.appendError("opencode create session: no id in response");
      await cancelBody(eventResponse.body);
      return failed();
    }
    sessionId = id;
  } catch (cause) {
    input.sink.appendError(
      `opencode create session error: ${String(cause).slice(0, 100)}`,
    );
    await cancelBody(eventResponse.body);
    return failed();
  }

  try {
    const prompted = await opencodeFetchWithTimeout(
      input.sandbox,
      "POST",
      `/session/${sessionId}/prompt_async`,
      {
        parts: [{ type: "text", text: input.text }],
        model: { providerID: input.provider, modelID: input.modelId },
        ...(input.toolPolicy
          ? { tools: huddlesPromptTools(input.toolPolicy) }
          : undefined),
        ...(input.outputFormat
          ? {
              format: {
                type: input.outputFormat.type,
                schema: input.outputFormat.schema,
                retryCount: input.outputFormat.retry_count,
              },
            }
          : undefined),
      },
      REQUEST_TIMEOUT_MS,
    );
    if (!prompted.ok) {
      const detail = safeHuddlesRuntimeDiagnostic(
        await prompted.text().catch(() => "response body unavailable"),
      );
      input.sink.appendError(
        `opencode prompt failed (HTTP ${prompted.status}): ${detail}`,
      );
      await cancelBody(eventResponse.body);
      return failed();
    }
  } catch (cause) {
    input.sink.appendError(
      `opencode prompt error: ${String(cause).slice(0, 100)}`,
    );
    await cancelBody(eventResponse.body);
    return failed();
  }

  const mapper = new OpencodeMapper();
  let appended = 0;
  let sawData = false;
  const iterator = sseEnvelopes(eventResponse.body)[Symbol.asyncIterator]();
  for (;;) {
    const step = await withTimeoutValue(
      iterator.next(),
      EVENT_IDLE_TIMEOUT_MS,
      "idle" as const,
    );
    if (step === "idle") {
      input.sink.appendError(
        sawData
          ? `agent turn stalled (no /event data for ${EVENT_IDLE_TIMEOUT_MS / 1_000}s)`
          : `no /event data in ${EVENT_IDLE_TIMEOUT_MS / 1_000}s — DO→container SSE not streaming`,
      );
      await iterator.return?.(undefined);
      return failed(appended);
    }
    if (step.done) return attemptResult(mapper, appended);

    sawData = true;
    let done = false;
    for (const payload of mapper.onEvent(step.value)) {
      if (appended >= input.eventBudget) {
        done = true;
        break;
      }
      input.sink.appendAgent(payload);
      appended += 1;
      if (payload.type === "attention_required") done = true;
      if (appended >= input.eventBudget) done = true;
    }
    if (done) {
      await iterator.return?.(undefined);
      return attemptResult(mapper, appended);
    }
    if (Date.now() > input.deadline) {
      input.sink.appendError(
        `agent turn exceeded ${TURN_TIMEOUT_MS / 1_000}s without going idle`,
      );
      await iterator.return?.(undefined);
      return failed(appended);
    }
  }
}

function attemptResult(
  mapper: OpencodeMapper,
  appended: number,
): OpencodeAttemptResult {
  return {
    structuredOutput: mapper.structuredOutput(),
    plainTextOutput: mapper.plainTextOutput(),
    mayRetryStructuredOutput: mapper.mayRetryStructuredOutput(),
    appended,
  };
}

function opencodeFetch(
  sandbox: SandboxHandle,
  method: string,
  path: string,
  jsonBody?: unknown,
  headers?: Readonly<Record<string, string>>,
): Promise<Response> {
  const requestHeaders: Record<string, string> = { ...headers };
  const init: RequestInit = { method };
  if (jsonBody !== undefined) {
    init.body = JSON.stringify(jsonBody);
    requestHeaders["content-type"] = "application/json";
  }
  if (Object.keys(requestHeaders).length > 0) init.headers = requestHeaders;
  return sandbox.containerFetch(
    new Request(`http://opencode${path}`, init),
    OPENCODE_PORT,
  );
}

function opencodeFetchWithTimeout(
  sandbox: SandboxHandle,
  method: string,
  path: string,
  jsonBody: unknown,
  timeoutMs: number,
): Promise<Response> {
  return withTimeout(
    opencodeFetch(sandbox, method, path, jsonBody),
    timeoutMs,
  );
}

async function probeDoc(
  sandbox: SandboxHandle,
): Promise<{ readonly ok: boolean; readonly detail: string }> {
  try {
    const response = await opencodeFetchWithTimeout(
      sandbox,
      "GET",
      "/doc",
      undefined,
      5_000,
    );
    return { ok: response.ok, detail: `HTTP ${response.status}` };
  } catch (cause) {
    return {
      ok: false,
      detail: `fetch ${String(cause).slice(0, 100)}`,
    };
  }
}

function splitModel(model: string): readonly [string, string | undefined] {
  const separator = model.indexOf("/");
  return separator === -1
    ? [model, undefined]
    : [model.slice(0, separator), model.slice(separator + 1)];
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function withTimeout<T>(
  operation: Promise<T>,
  timeoutMs: number,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`timeout ${timeoutMs}ms`)),
          timeoutMs,
        );
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

async function withTimeoutValue<T, Timeout>(
  operation: Promise<T>,
  timeoutMs: number,
  timeoutValue: Timeout,
): Promise<T | Timeout> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<Timeout>((resolve) => {
        timer = setTimeout(() => resolve(timeoutValue), timeoutMs);
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

async function cancelBody(body: ReadableStream<Uint8Array>): Promise<void> {
  try {
    await body.cancel();
  } catch (cause) {
    console.warn(
      "opencode event stream cleanup failed",
      safeHuddlesRuntimeDiagnostic(cause),
    );
  }
}

// Parse OpenCode `/event` SSE frames into JSON envelopes. Workers fetch already
// de-chunks the transport, so this handles only SSE framing and closes the reader
// when a bounded turn ends early.
async function* sseEnvelopes(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<unknown> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let data = "";
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (value) buffer += decoder.decode(value, { stream: true });
      let newline: number;
      while ((newline = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, newline).replace(/\r$/, "");
        buffer = buffer.slice(newline + 1);
        if (line.startsWith("data:")) {
          const rest = line.slice(5);
          if (data !== "") data += "\n";
          data += rest.startsWith(" ") ? rest.slice(1) : rest;
        } else if (line === "" && data !== "") {
          const frame = data;
          data = "";
          try {
            yield JSON.parse(frame);
          } catch {
            // Skip non-JSON frames, matching the local OpenCode driver.
          }
        }
      }
      if (done) break;
    }
    if (data !== "") {
      try {
        yield JSON.parse(data);
      } catch {
        // Ignore an incomplete non-JSON terminal frame.
      }
    }
  } finally {
    try {
      await reader.cancel();
    } catch {
      // The stream may already be closed.
    }
  }
}
