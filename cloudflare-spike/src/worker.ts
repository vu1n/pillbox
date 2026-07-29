import { routeAgentRequest, type AgentOptions } from "agents";
import { proxyToSandbox, type Sandbox } from "@cloudflare/sandbox";
import { SessionGateway } from "./session_gateway.js";
import {
  HuddlesRuntimeEntrypoint,
  isHuddlesSessionName,
} from "./huddles_runtime.js";

export { SessionGateway };
// Named entrypoint: Huddles reaches ensureSession through a same-account
// service-binding RPC. The default fetch handler below never routes that method.
export { HuddlesRuntimeEntrypoint };
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

  // opencode provider auth + model for the consume path (driveAgent). Set via
  // `wrangler secret put` / `.dev.vars`; consumed by createOpencodeServer
  // (managed-tier Milestone 2 — the managed secret store, NOT our MITM vault).
  // Known provider keys are passed through as env so opencode auto-detects them.
  ANTHROPIC_API_KEY?: string;
  OPENAI_API_KEY?: string;
  // Z.AI GLM coding-plan subscription key — wired into opencode's `zai-coding-plan`
  // provider (a config overlay, since it isn't a standard-env auto-detect provider).
  ZAI_API_KEY?: string;
  // Full opencode `config` JSON (a provider block with an apiKey, or a CF AI
  // Gateway) — an alternative to / override of the key env vars above.
  OPENCODE_CONFIG_JSON?: string;
  // Default model (`provider/modelID`) when an /input doesn't carry one.
  OPENCODE_MODEL?: string;
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const requirePublicSession: NonNullable<
      AgentOptions<Env>["onBeforeRequest"]
    > = async (_request, lobby) => {
      if (lobby.className !== "SessionGateway") return;
      // The deterministic Huddles namespace is private before its durable
      // binding exists, so a public caller cannot pre-bind a permissive server.
      if (isHuddlesSessionName(lobby.name)) {
        return new Response("not found\n", { status: 404 });
      }
      const id = env.SessionGateway.idFromName(lobby.name);
      if (!(await env.SessionGateway.get(id).publicAccessAllowed())) {
        return new Response("not found\n", { status: 404 });
      }
    };
    // Container preview/port-proxy URLs — only when the container is bound.
    // Re-spread with the narrowed (defined) Sandbox so proxyToSandbox's env type
    // is satisfied without a cast (TS narrows `env.Sandbox`, not `env`).
    if (env.Sandbox) {
      const proxied = await proxyToSandbox(req, {
        ...env,
        Sandbox: env.Sandbox,
      });
      if (proxied) return proxied;
    }
    // Route to the per-session §0 gateway Agent (works with or without a container).
    return (
      (await routeAgentRequest(req, env, {
        onBeforeConnect: requirePublicSession,
        onBeforeRequest: requirePublicSession,
      })) ?? new Response("not found\n", { status: 404 })
    );
  },
};
