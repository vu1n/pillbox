import type {
  EnsureSessionRequest,
  EnsureSessionResult,
  EvidenceReadRequest,
  InvokeSessionRequest,
  InvokeSessionResult,
} from "../src/huddles_runtime.js";
import type { EvidenceFrame } from "../src/evidence_reader.js";

// Test-only HTTP bridge into the private service binding. The product Worker
// intentionally has no equivalent route.
interface Env {
  PillboxRuntime: {
    ensureSession(request: EnsureSessionRequest): Promise<EnsureSessionResult>;
    invokeSession(request: InvokeSessionRequest): Promise<InvokeSessionResult>;
    readEvidence(request: EvidenceReadRequest): Promise<readonly EvidenceFrame[]>;
  };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (path !== "/ensure" && path !== "/invoke" && path !== "/evidence") {
      return new Response("not found\n", { status: 404 });
    }
    try {
      const input = await request.json();
      let result: EnsureSessionResult | InvokeSessionResult | readonly EvidenceFrame[];
      if (path === "/ensure") {
        result = await env.PillboxRuntime.ensureSession(input as EnsureSessionRequest);
      } else if (path === "/invoke") {
        result = await env.PillboxRuntime.invokeSession(input as InvokeSessionRequest);
      } else {
        result = await env.PillboxRuntime.readEvidence(input as EvidenceReadRequest);
      }
      if (!Array.isArray(result) && "code" in result)
        return Response.json({ error: result }, { status: 409 });
      return Response.json(result);
    } catch (error) {
      const detail = error as {
        code?: string;
        message?: string;
        name?: string;
      };
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
