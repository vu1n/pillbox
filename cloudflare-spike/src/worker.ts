import { SessionGateway } from "./session_gateway.js";

export { SessionGateway };

export interface Env {
  SESSION: DurableObjectNamespace<SessionGateway>;
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    // Routes: /s/<sessionId>/event | /subscribe | /input
    const m = url.pathname.match(/^\/s\/([^/]+)\/(event|subscribe|input)$/);
    if (!m) return new Response("not found\n", { status: 404 });

    const [, sessionId, op] = m;
    // One DO per session — the partition key IS the DO name.
    const id = env.SESSION.idFromName(sessionId);
    const stub = env.SESSION.get(id);

    // Rewrite to the DO's internal path (it ignores the sessionId; it IS one).
    const inner = new URL(req.url);
    inner.pathname = `/${op}`;
    return stub.fetch(new Request(inner, req));
  },
};
