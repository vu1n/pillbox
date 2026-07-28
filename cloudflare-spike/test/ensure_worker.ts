import type {
  EnsureSessionRequest,
  EnsureSessionResult,
} from "../src/huddles_runtime.js";

// Test-only HTTP bridge into the private service binding. The product Worker
// intentionally has no equivalent route.
interface Env {
  PillboxRuntime: {
    ensureSession(request: EnsureSessionRequest): Promise<EnsureSessionResult>;
  };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (new URL(request.url).pathname !== "/ensure") return new Response("not found\n", { status: 404 });
    try {
      const input = (await request.json()) as EnsureSessionRequest;
      const result = await env.PillboxRuntime.ensureSession(input);
      if ("code" in result) return Response.json({ error: result }, { status: 409 });
      return Response.json(result);
    } catch (error) {
      const detail = error as { code?: string; message?: string; name?: string };
      return Response.json(
        { error: { code: detail.code, message: detail.message, name: detail.name } },
        { status: 409 },
      );
    }
  },
};
