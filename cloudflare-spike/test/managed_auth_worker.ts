import { WorkerEntrypoint } from "cloudflare:workers";

const currentnessCalls: unknown[] = [];

/**
 * Test-only Huddles authority. It accepts only the exact generic v3
 * currentness envelope, so stale or downgraded callers fail closed.
 */
export class PillboxAuthorizationCurrentnessEntrypoint extends WorkerEntrypoint {
  authorizeExecutionOperationGrant(input: unknown): unknown {
    assertCurrentnessV3(input);
    currentnessCalls.push(input);
    const request = input as {
      grant: { claims: { grant_id: string } };
    };
    if (request.grant.claims.grant_id.includes("revoked")) {
      throw new Error("operation grant is revoked");
    }
    return request.grant.claims;
  }

}

function assertCurrentnessV3(value: unknown): asserts value is {
  version: "pillbox.authorization-currentness/3";
  grant: {
    algorithm: "Ed25519";
    key_id: string;
    claims: Record<string, unknown>;
    signature: string;
  };
  expected: { operation: string; invocation_id: string; request_digest: string };
  verified_signer: {
    algorithm: "Ed25519";
    key_id: string;
    public_key_sha256: string;
  };
} {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("operation currentness request must be an object");
  }
  const request = value as Record<string, unknown>;
  if (request.version !== "pillbox.authorization-currentness/3") {
    throw new Error("operation currentness must use version 3");
  }
  const grant = request.grant as Record<string, unknown> | undefined;
  if (
    !grant ||
    grant.algorithm !== "Ed25519" ||
    grant.key_id !== "test-key" ||
    typeof grant.signature !== "string" ||
    !grant.claims
  ) {
    throw new Error("currentness v3 must carry the exact signed grant envelope");
  }
  const expected = request.expected as Record<string, unknown> | undefined;
  if (
    !expected ||
    !["execute", "status", "cancel"].includes(expected.operation as string) ||
    typeof expected.invocation_id !== "string" ||
    !/^sha256:[0-9a-f]{64}$/.test(expected.request_digest as string)
  ) {
    throw new Error("operation currentness expected binding is invalid");
  }
  assertVerifiedSigner(request.verified_signer);
}

export default {
  fetch(request: Request): Response {
    if (new URL(request.url).pathname === "/calls") {
      return Response.json(currentnessCalls);
    }
    return new Response("not found\n", { status: 404 });
  },
};

function assertVerifiedSigner(signer: unknown): void {
  if (!signer || typeof signer !== "object" || Array.isArray(signer)) {
    throw new Error("verified signer is required");
  }
  const verifiedSigner = signer as Record<string, unknown>;
  if (
    verifiedSigner.algorithm !== "Ed25519" ||
    verifiedSigner.key_id !== "test-key" ||
    verifiedSigner.public_key_sha256 !==
      "sha256:be7c33f790cd7e862fbafca20d617cc3dd30c4d5785921a788124cebd7ffdf6b"
  ) {
    throw new Error("currentness signer identity is wrong");
  }
}
