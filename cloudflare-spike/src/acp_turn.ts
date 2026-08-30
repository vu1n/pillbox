import {
  CodexExecutionBoundaryError,
  computeExecutionIdentityDigest,
  type ExecuteInvocationV2Request,
  type ExecutionDigest,
  type JsonValue,
  validateSupportedAcpExecution,
} from "./codex_execution.js";
import { inspectRawStructuredOutput } from "./structured_output.js";

/** Logical ACP session creation parameters. The transport owns protocol encoding. */
export interface AcpSessionNewParams {
  readonly mcpServers: readonly [];
}

export interface AcpSession {
  readonly session_id: string;
}

/** The only invocation data the ACP prompt transport receives. */
export interface AcpPromptParams {
  readonly session_id: string;
  readonly text: string;
  readonly on_event: (event: JsonValue) => void;
}

export interface AcpPromptResult {
  readonly output?: JsonValue;
}

/** An injected ACP process/client seam; no Cloudflare or subprocess code is required here. */
export interface AcpClient {
  readonly initialize: () => Promise<void>;
  readonly session_new: (params: AcpSessionNewParams) => Promise<AcpSession>;
  readonly prompt: (params: AcpPromptParams) => Promise<AcpPromptResult>;
  readonly cancel: (params: { readonly session_id: string }) => Promise<void>;
  /** Bounded cleanup/kill hook. The driver applies its own timeout. */
  readonly cleanup: () => Promise<void>;
  /** Recreate a crashed or cancelled process for a later invocation only. */
  readonly respawn: () => Promise<void>;
}

export interface AcpEventAttribution {
  readonly session_ref: { readonly session_id: string };
  readonly invocation_id: string;
  readonly execution_digest: ExecutionDigest;
  readonly execution_policy_revision: string;
}

/** Generic event evidence; this adapter does not create HCP or WorkEvent records. */
export interface AcpEventEnvelope {
  readonly attribution: AcpEventAttribution;
  readonly event: JsonValue;
}

export interface AcpTurnSink {
  readonly appendAcpEvent: (event: AcpEventEnvelope) => void;
}

export type AcpTurnFailureCode =
  | "runtime_busy"
  | "runtime_interrupted"
  | "runtime_failed"
  | "cancelled"
  | "structured_output_missing";

export type AcpTurnResult =
  | {
      readonly status: "completed";
      readonly attribution: AcpEventAttribution;
      readonly output?: JsonValue;
    }
  | {
      readonly status: "failed" | "cancelled";
      readonly attribution?: AcpEventAttribution;
      readonly error: {
        readonly code: AcpTurnFailureCode;
        readonly message: string;
      };
    };

export class AcpProcessCrashedError extends Error {
  readonly code = "acp_process_crashed" as const;

  constructor(message = "ACP child process exited during the invocation") {
    super(message);
    this.name = "AcpProcessCrashedError";
  }
}

const DEFAULT_CLEANUP_TIMEOUT_MS = 2_000;

interface ActiveTurn {
  readonly request: ExecuteInvocationV2Request;
  readonly sink: AcpTurnSink;
  readonly attributionPromise: Promise<AcpEventAttribution>;
  session_id?: string;
  cancel_requested: boolean;
  cancel_promise?: Promise<void>;
  cancellation_error?: string;
  result_promise?: Promise<AcpTurnResult>;
}

/**
 * Runs exactly one sealed invocation at a time over an injected ACP client.
 * A crashed/cancelled client is respawned only when a later invocation starts;
 * the current invocation is never transparently retried.
 */
export class AcpTurnDriver {
  private active?: ActiveTurn;
  private respawn_required = false;
  private readonly client: AcpClient;
  private readonly cleanup_timeout_ms: number;

  constructor(client: AcpClient, cleanup_timeout_ms = DEFAULT_CLEANUP_TIMEOUT_MS) {
    this.client = client;
    this.cleanup_timeout_ms = cleanup_timeout_ms;
  }

  get busy(): boolean {
    return this.active !== undefined;
  }

  /** Execute a request already validated by the pillbox.execution/2 boundary. */
  execute(
    request: ExecuteInvocationV2Request,
    sink: AcpTurnSink,
  ): Promise<AcpTurnResult> {
    validateSupportedAcpExecution(request.execution);
    if (request.tool_policy !== "deny_all") {
      throw new CodexExecutionBoundaryError(
        "ACP adapter only accepts the sealed deny_all tool policy",
      );
    }
    if (this.active !== undefined) {
      return Promise.resolve(
        failure("runtime_busy", "an ACP invocation is already active"),
      );
    }

    const active: ActiveTurn = {
      request,
      sink,
      attributionPromise: this.makeAttribution(request),
      cancel_requested: false,
    };
    this.active = active;
    const result = this.run(active);
    active.result_promise = result;
    return result;
  }

  /**
   * Request cancellation of the active turn. ACP cancellation is sent before
   * the bounded cleanup/kill hook, and the result remains a cancellation even
   * when cleanup reports a diagnostic.
   */
  async cancelActiveTurn(): Promise<AcpTurnResult | undefined> {
    const active = this.active;
    if (active === undefined) return undefined;

    active.cancel_requested = true;
    // If session/new is still in flight, let the run record its session id
    // first so cancellation always reaches ACP before cleanup.
    if (active.session_id !== undefined) await this.beginCancellation(active);
    return active.result_promise;
  }

  private async run(active: ActiveTurn): Promise<AcpTurnResult> {
    let attribution: AcpEventAttribution | undefined;
    try {
      attribution = await active.attributionPromise;
      if (active.cancel_requested) {
        await this.beginCancellation(active);
        return cancelled(attribution, active.cancellation_error);
      }

      if (this.respawn_required) {
        await this.client.respawn();
        this.respawn_required = false;
      }
      await this.client.initialize();
      if (active.cancel_requested) {
        await this.beginCancellation(active);
        return cancelled(attribution, active.cancellation_error);
      }

      const session = await this.client.session_new({ mcpServers: [] });
      if (session.session_id.length === 0) {
        throw new Error("ACP session/new returned an empty session id");
      }
      active.session_id = session.session_id;
      if (active.cancel_requested) {
        await this.beginCancellation(active);
        return cancelled(attribution, active.cancellation_error);
      }

      const prompted = await this.client.prompt({
        session_id: session.session_id,
        text: active.request.rendered_input,
        on_event: (event) => {
          active.sink.appendAcpEvent({ attribution: attribution!, event });
        },
      });
      if (active.cancel_requested) {
        await this.beginCancellation(active);
        return cancelled(attribution, active.cancellation_error);
      }
      if (active.request.output_format.type !== "json_schema") {
        return failure(
          "runtime_failed",
          "ACP adapter requires json_schema output",
          attribution,
        );
      }
      const output = inspectRawStructuredOutput(
        prompted.output === undefined ? undefined : JSON.stringify(prompted.output),
        active.request.output_format.schema,
      );
      if (output.status === "rejected") {
        if (output.reason === "missing") {
          return failure(
            "structured_output_missing",
            "ACP prompt returned no structured output",
            attribution,
          );
        }
        return failure(
          "runtime_failed",
          `ACP structured output rejected: ${output.detail}`,
          attribution,
        );
      }
      return { status: "completed", attribution, output: JSON.parse(output.output) };
    } catch (cause) {
      if (active.cancel_requested) {
        await this.beginCancellation(active);
        return cancelled(attribution, active.cancellation_error, cause);
      }
      if (isCrash(cause)) {
        this.respawn_required = true;
        const cleanup_error = await this.cleanupAfterFailure();
        const suffix = cleanup_error ? `; cleanup: ${cleanup_error}` : "";
        return failure(
          "runtime_interrupted",
          `${errorMessage(cause)}${suffix}`,
          attribution,
        );
      }
      return failure("runtime_failed", errorMessage(cause), attribution);
    } finally {
      if (this.active === active) this.active = undefined;
    }
  }

  private async beginCancellation(active: ActiveTurn): Promise<void> {
    if (active.cancel_promise !== undefined) return active.cancel_promise;
    active.cancel_promise = (async () => {
      let diagnostics: string[] = [];
      if (active.session_id !== undefined) {
        try {
          await withTimeout(
            this.client.cancel({ session_id: active.session_id }),
            this.cleanup_timeout_ms,
          );
        } catch (cause) {
          diagnostics.push(`cancel: ${errorMessage(cause)}`);
        }
      }
      try {
        await withTimeout(this.client.cleanup(), this.cleanup_timeout_ms);
      } catch (cause) {
        diagnostics.push(`cleanup: ${errorMessage(cause)}`);
      }
      // Cleanup is allowed to kill the ACP child, so the next invocation must
      // explicitly respawn it. Nothing here retries the cancelled request.
      this.respawn_required = true;
      if (diagnostics.length > 0) active.cancellation_error = diagnostics.join("; ");
    })();
    return active.cancel_promise;
  }

  private async cleanupAfterFailure(): Promise<string | undefined> {
    try {
      await withTimeout(this.client.cleanup(), this.cleanup_timeout_ms);
      return undefined;
    } catch (cause) {
      return errorMessage(cause);
    }
  }

  private async makeAttribution(
    request: ExecuteInvocationV2Request,
  ): Promise<AcpEventAttribution> {
    return {
      session_ref: { session_id: request.session_ref.session_id },
      invocation_id: request.invocation_id,
      execution_digest: await computeExecutionIdentityDigest(
        request.execution,
        request.execution_policy_revision,
      ),
      execution_policy_revision: request.execution_policy_revision,
    };
  }
}

export async function driveAcpTurn(input: {
  readonly client: AcpClient;
  readonly request: ExecuteInvocationV2Request;
  readonly sink: AcpTurnSink;
  readonly cleanup_timeout_ms?: number;
}): Promise<AcpTurnResult> {
  return new AcpTurnDriver(input.client, input.cleanup_timeout_ms).execute(
    input.request,
    input.sink,
  );
}

function failure(
  code: AcpTurnFailureCode,
  message: string,
  attribution?: AcpEventAttribution,
): AcpTurnResult {
  return {
    status: code === "cancelled" ? "cancelled" : "failed",
    ...(attribution === undefined ? {} : { attribution }),
    error: { code, message },
  };
}

function cancelled(
  attribution: AcpEventAttribution | undefined,
  cleanup_error?: string,
  cause?: unknown,
): AcpTurnResult {
  const detail = cleanup_error
    ? `; ${cleanup_error}`
    : cause === undefined
      ? ""
      : `; ${errorMessage(cause)}`;
  return failure("cancelled", `ACP turn cancelled${detail}`, attribution);
}

function isCrash(cause: unknown): boolean {
  if (cause instanceof AcpProcessCrashedError) return true;
  if (typeof cause !== "object" || cause === null) return false;
  const value = cause as { readonly code?: unknown; readonly kind?: unknown };
  return value.code === "acp_process_crashed" || value.kind === "crash";
}

function errorMessage(cause: unknown): string {
  if (cause instanceof Error && cause.message.length > 0) return cause.message;
  return String(cause).slice(0, 240);
}

function withTimeout<T>(promise: Promise<T>, timeout_ms: number): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`ACP cleanup exceeded ${timeout_ms}ms`)),
      timeout_ms,
    );
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (cause) => {
        clearTimeout(timer);
        reject(cause);
      },
    );
  });
}
