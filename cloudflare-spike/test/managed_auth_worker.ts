import { WorkerEntrypoint } from "cloudflare:workers";

const currentnessCalls: unknown[] = [];

/**
 * Test-only Huddles authority. It deliberately accepts only currentness v2 so
 * the boundary test fails if Pillbox silently retries the old v1 request.
 */
export class ManagedAuthorizationControlPlaneEntrypoint extends WorkerEntrypoint {
  authorizeExecutionGrant(input: unknown): unknown {
    assertCurrentnessV2(input);
    currentnessCalls.push(input);
    return (input as { grant: unknown }).grant;
  }

}

export default {
  fetch(request: Request): Response {
    if (new URL(request.url).pathname === "/calls") {
      return Response.json(currentnessCalls);
    }
    return new Response("not found\n", { status: 404 });
  },
};

function assertCurrentnessV2(value: unknown): asserts value is {
  version: "pillbox.authorization-currentness/2";
  grant: unknown;
  verified_signer: {
    algorithm: "Ed25519";
    key_id: string;
    public_key_sha256: string;
  };
} {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("currentness request must be an object");
  }
  const request = value as Record<string, unknown>;
  if (request.version !== "pillbox.authorization-currentness/2") {
    throw new Error("currentness v1 is not accepted by this test authority");
  }
  const signer = request.verified_signer;
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
