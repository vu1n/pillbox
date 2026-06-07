import { routeAgentRequest } from "agents";
import { proxyToSandbox, type Sandbox } from "@cloudflare/sandbox";
import { SessionGateway } from "./session_gateway.js";

export { SessionGateway };
// Re-export the SDK's container-owning DO so wrangler can bind it.
export { Sandbox } from "@cloudflare/sandbox";

export interface Env {
  // The §0 gateway Agent (kebab-class `session-gateway` in the route).
  SessionGateway: DurableObjectNamespace<SessionGateway>;
  // The Sandbox SDK's container DO — OPTIONAL: present only in the container
  // config (wrangler.container.toml). Absent in the free/§0-only deploy.
  Sandbox?: DurableObjectNamespace<Sandbox>;
  // Issuer secret for verifying actor tokens (HMAC). Set out-of-band via
  // `wrangler secret put ACTOR_TOKEN_SECRET` (or `.dev.vars` for `wrangler dev`),
  // never committed. Absent → writes fail closed (no actor can be attested).
  ACTOR_TOKEN_SECRET?: string;
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    // Container preview/port-proxy URLs — only when the container is bound.
    // Re-spread with the narrowed (defined) Sandbox so proxyToSandbox's env type
    // is satisfied without a cast (TS narrows `env.Sandbox`, not `env`).
    if (env.Sandbox) {
      const proxied = await proxyToSandbox(req, { ...env, Sandbox: env.Sandbox });
      if (proxied) return proxied;
    }
    // Route to the per-session §0 gateway Agent (works with or without a container).
    return (await routeAgentRequest(req, env)) ?? new Response("not found\n", { status: 404 });
  },
};
