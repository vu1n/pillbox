import type {
  ExecuteInvocationV2Request,
  CancelInvocationV2Request,
  ExecutionAttribution,
} from "./codex_execution.js";
import type { ExecutionRecord } from "./execution_store.js";
import type { ExecutionRuntime } from "./execution_service.js";
import { DIGITALOCEAN_TRANSPORT } from "./digitalocean_contract.js";
import type { DigitalOceanExecutionRuntime } from "./digitalocean_runtime.js";

/** Resolve only the sealed transport; deployment changes never provide cross-provider fallback. */
export class ManagedExecutionRuntime implements ExecutionRuntime {
  private readonly cloudflare: ExecutionRuntime;
  private readonly digitalocean: DigitalOceanExecutionRuntime;
  constructor(cloudflare: ExecutionRuntime, digitalocean: DigitalOceanExecutionRuntime) {
    this.cloudflare = cloudflare;
    this.digitalocean = digitalocean;
  }
  preflight(request: ExecuteInvocationV2Request) {
    return request.execution.transport.transport === DIGITALOCEAN_TRANSPORT
      ? this.digitalocean.preflight(request)
      : this.cloudflare.preflight?.(request);
  }
  execute(request: ExecuteInvocationV2Request) {
    return (
      request.execution.transport.transport === DIGITALOCEAN_TRANSPORT
        ? this.digitalocean
        : this.cloudflare
    ).execute(request);
  }
  cancel(
    request: CancelInvocationV2Request,
    sessionId: string,
    attribution?: ExecutionAttribution,
  ) {
    return attribution?.transport === DIGITALOCEAN_TRANSPORT
      ? this.digitalocean.cancel(request)
      : this.cloudflare.cancel(request, sessionId);
  }
  async reconcile(record: ExecutionRecord): Promise<void> {
    if (record.attribution.transport === DIGITALOCEAN_TRANSPORT)
      await this.digitalocean.cleanup(record.invocation_id);
  }
}
