import type {
  ExecuteInvocationV2Request,
  InvocationExecution,
} from "./codex_execution.js";
import type { InvokeSessionRequest } from "./huddles_runtime.js";

export function legacyExecutionRequest(
  request: InvokeSessionRequest,
): ExecuteInvocationV2Request {
  const slash = request.requested_model.indexOf("/");
  const provider = slash > 0 ? request.requested_model.slice(0, slash) : "custom";
  const model =
    slash > 0 ? request.requested_model.slice(slash + 1) : request.requested_model;
  const execution: InvocationExecution = {
    transport: {
      harness: "opencode",
      transport: "http",
      harness_version: "legacy",
      adapter_revision: "huddles-compat/1",
    },
    requested: {
      provider,
      model,
      profile: null,
      reasoning_effort: "medium",
    },
    placement: "managed_container",
    context_renderer_revision: "huddles-compat/1",
  };
  return {
    contract_version: "pillbox.execution/2",
    session_ref: request.session_ref,
    invocation_id: request.invocation_id,
    idempotency_key: request.delivery_receipt_id,
    rendered_input: request.rendered_input,
    rendered_input_hash: request.rendered_input_hash as `sha256:${string}`,
    tool_policy: request.tool_policy,
    execution,
    execution_policy_revision: "huddles-compat/1",
    output_format: request.output_format,
  };
}
