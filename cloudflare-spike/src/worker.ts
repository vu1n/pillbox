import { routeAgentRequest } from "agents";
import { proxyToSandbox, type Sandbox } from "@cloudflare/sandbox";
import { SessionGateway } from "./session_gateway.js";

export { SessionGateway };
// Re-export the SDK's container-owning DO so wrangler can bind it.
export { Sandbox } from "@cloudflare/sandbox";

export interface Env {
  // The §0 gateway Agent (kebab-class `session-gateway` in the route).
  SessionGateway: DurableObjectNamespace<SessionGateway>;
  // The Sandbox SDK's container DO — the gateway drives it via getSandbox().
  Sandbox: DurableObjectNamespace<Sandbox>;
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    // The Sandbox SDK claims its own preview/port-proxy URLs first.
    const proxied = await proxyToSandbox(req, env);
    if (proxied) return proxied;
    // Otherwise route to the per-session §0 gateway Agent.
    return (await routeAgentRequest(req, env)) ?? new Response("not found\n", { status: 404 });
  },
};
