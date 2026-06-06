import { routeAgentRequest } from "agents";
import { SessionGateway } from "./session_gateway.js";

export { SessionGateway };

export interface Env {
  // The Agents SDK addresses the agent by binding; the kebab-cased class name
  // (`session-gateway`) is the URL segment. routeAgentRequest maps
  // /agents/session-gateway/<sessionId>/* → this DO (HTTP → onRequest, WS → onConnect).
  SessionGateway: DurableObjectNamespace<SessionGateway>;
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    return (await routeAgentRequest(req, env)) ?? new Response("not found\n", { status: 404 });
  },
};
