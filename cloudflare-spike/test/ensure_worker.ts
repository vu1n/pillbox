import type {
  EnsureSessionRequest,
  EnsureSessionResult,
  InvokeSessionRequest,
  InvokeSessionResult,
} from "../src/legacy_huddles_adapter.js";

// Test-only HTTP bridge into the private service binding. The product Worker
// intentionally has no equivalent route.
interface Env {
  PillboxRuntime: {
    ensureSession(request: EnsureSessionRequest): Promise<EnsureSessionResult>;
    invokeSession(request: InvokeSessionRequest): Promise<InvokeSessionResult>;
  };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (path !== "/ensure" && path !== "/invoke") {
      return new Response("not found\n", { status: 404 });
    }
    try {
      const input = await request.json();
      const result =
        path === "/ensure"
          ? await env.PillboxRuntime.ensureSession(input as EnsureSessionRequest)
          : await env.PillboxRuntime.invokeSession(input as InvokeSessionRequest);
      if ("code" in result) {
        return Response.json({ error: result }, { status: 409 });
      }
      return Response.json(result);
    } catch (error) {
      const detail = error as { code?: string; message?: string; name?: string };
      return Response.json(
        {
          error: {
            code: detail.code,
            message: detail.message,
            name: detail.name,
          },
        },
        { status: 409 },
      );
    }
  },
};
