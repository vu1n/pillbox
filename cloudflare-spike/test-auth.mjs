import assert from "node:assert/strict";
import { test } from "node:test";
import {
  bearerToken,
  signManagedCapability,
  verifyManagedCapability,
} from "./src/auth.ts";

const secret = "test-secret-123";
const now = 1_700_000_000_000;
const capability = {
  version: 1,
  subject: "controller:alice",
  audience: "pillbox-managed",
  expires_at_ms: now + 60_000,
  operation: "execute",
  session_id: "abc123def456",
  invocation_id: "def456abc123",
};
const scope = {
  operation: "execute",
  session_id: "abc123def456",
  invocation_id: "def456abc123",
};

test("managed capabilities are exact, expiring operation grants", async () => {
  const token = await signManagedCapability(capability, secret);
  assert.deepEqual(await verifyManagedCapability(token, secret, scope, now), capability);
  assert.equal(await verifyManagedCapability(token, "wrong", scope, now), null);
  assert.equal(
    await verifyManagedCapability(token, secret, { ...scope, operation: "status" }, now),
    null,
  );
  assert.equal(
    await verifyManagedCapability(token, secret, { ...scope, session_id: "other" }, now),
    null,
  );
  assert.equal(
    await verifyManagedCapability(token, secret, { ...scope, invocation_id: "other" }, now),
    null,
  );
  assert.equal(await verifyManagedCapability(token, secret, scope, capability.expires_at_ms), null);
  const farFuture = await signManagedCapability(
    { ...capability, expires_at_ms: now + 16 * 60 * 1000 },
    secret,
  );
  assert.equal(await verifyManagedCapability(farFuture, secret, scope, now), null);
});

test("managed capabilities reject the wrong audience and malformed tokens", async () => {
  const wrongAudience = await signManagedCapability(
    { ...capability, audience: "another-service" },
    secret,
  );
  assert.equal(await verifyManagedCapability(wrongAudience, secret, scope, now), null);
  for (const token of ["", "garbage", "a.", ".b", "a.b.c", "%%%.$$$"]) {
    assert.equal(await verifyManagedCapability(token, secret, scope, now), null);
  }
});

test("bearer extraction accepts one opaque token only", () => {
  assert.equal(
    bearerToken(
      new Request("https://x/", { headers: { authorization: "Bearer abc.def" } }),
    ),
    "abc.def",
  );
  assert.equal(
    bearerToken(
      new Request("https://x/", { headers: { authorization: "Bearer abc def" } }),
    ),
    null,
  );
  assert.equal(bearerToken(new Request("https://x/")), null);
});
