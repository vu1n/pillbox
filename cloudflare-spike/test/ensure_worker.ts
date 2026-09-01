import type {
  CancelInvocationV2Request,
  ExecuteInvocationV2Request,
  ExecuteInvocationV2Result,
  GetInvocationV2Request,
} from "../src/codex_execution.js";
import type { PillboxExecutionOperationGrantIssueResponse } from "../src/managed_contract.js";

// Test-only HTTP bridge into the private service binding. The product Worker
// intentionally has no equivalent route.
interface Env {
  PillboxRuntime: {
    executeInvocation(
      request: ExecuteInvocationV2Request,
      authorization: PillboxExecutionOperationGrantIssueResponse,
    ): Promise<ExecuteInvocationV2Result>;
    getExecutionStatus(
      request: GetInvocationV2Request,
      authorization: PillboxExecutionOperationGrantIssueResponse,
    ): Promise<ExecuteInvocationV2Result>;
    cancelInvocation(
      request: CancelInvocationV2Request,
      authorization: PillboxExecutionOperationGrantIssueResponse,
    ): Promise<ExecuteInvocationV2Result>;
  };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (!["/execute", "/status", "/cancel"].includes(path)) {
      return new Response("not found\n", { status: 404 });
    }
    try {
      const input = await request.json() as {
        request?: unknown;
        authorization?: PillboxExecutionOperationGrantIssueResponse;
      };
      if (path === "/execute") {
        return Response.json(await env.PillboxRuntime.executeInvocation(
          input.request as ExecuteInvocationV2Request,
          input.authorization as PillboxExecutionOperationGrantIssueResponse,
        ));
      }
      if (path === "/status") {
        return Response.json(await env.PillboxRuntime.getExecutionStatus(
          input.request as GetInvocationV2Request,
          input.authorization as PillboxExecutionOperationGrantIssueResponse,
        ));
      }
      if (path === "/cancel") {
        return Response.json(await env.PillboxRuntime.cancelInvocation(
          input.request as CancelInvocationV2Request,
          input.authorization as PillboxExecutionOperationGrantIssueResponse,
        ));
      }
      return new Response("not found\n", { status: 404 });
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
