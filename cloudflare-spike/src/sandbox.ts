import { getSandbox, Sandbox as CloudflareSandbox, ContainerProxy } from "@cloudflare/sandbox";
import type { Env } from "./worker.js";
import { authorizeExecutionGrant } from "./managed_auth.js";
import { fetchWithCredentialLease } from "./outbound/credential_handler.js";
import type { ManagedOutboundParams } from "./credentials/contract.js";

// The outbound-handler API is shipped by the Sandbox runtime/container pair.
// Keep these local types until the package's public declarations catch up with
// the runtime API; the casts below are deliberately limited to that seam.
type OutboundHandlerContext<P> = { readonly params?: P; readonly containerId?: string };
type OutboundHandler<P> = (request: Request, env: Env, ctx: OutboundHandlerContext<P>) => Promise<Response>;
type ManagedSandboxClass = {
  outboundHandlers: Record<string, OutboundHandler<ManagedOutboundParams>>;
};

export type ManagedSandboxPolicy = Sandbox & {
  setAllowedHosts(hosts: readonly string[]): Promise<void>;
  setOutboundByHost(host: string, handlerName: string, params: ManagedOutboundParams): Promise<void>;
};

/** Managed containers have no public internet; only configured broker hosts egress. */
export class Sandbox extends CloudflareSandbox<Env> {
  enableInternet = false;
  interceptHttps = true;
}

// The SDK uses this class-level registry when ContainerProxy is exported from
// the Worker entrypoint. See the runtime API note above.
const managedSandboxClass = Sandbox as unknown as ManagedSandboxClass;
managedSandboxClass.outboundHandlers = {
  managedCredential: async (request: Request, env: Env, ctx: OutboundHandlerContext<ManagedOutboundParams>) => {
    const params = ctx.params;
    if (!params) return new Response("managed outbound authorization required\n", { status: 403 });
    const claims = await authorizeExecutionGrant(env, params.grant, params.expected);
    const binding = claims.runtime_policy.credential_bindings.find((candidate) => candidate.credential_binding_id === params.route.credential_binding_id && candidate.secret_ref === params.route.secret_ref && candidate.purpose === params.route.purpose);
    if (!binding) return new Response("managed credential binding denied\n", { status: 403 });
    const brokerNamespace = env.CredentialBroker;
    if (!brokerNamespace) return new Response("managed credential broker unavailable\n", { status: 503 });
    const broker = brokerNamespace.get(brokerNamespace.idFromName(params.route.credential_binding_id));
    return fetchWithCredentialLease({ request, leaseRequest: params, lease: () => broker.lease(params) });
  },
};

export { ContainerProxy };

export function getManagedSandbox(namespace: DurableObjectNamespace<Sandbox>, id: string): ManagedSandboxPolicy {
  return getSandbox(namespace, id) as ManagedSandboxPolicy;
}
