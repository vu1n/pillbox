// Unit test for the actor-token crypto (no worker needed). Run: node test-auth.mjs
// (Node >= 23 strips the .ts types on import; auth.ts has only a type-only import.)
import assert from "node:assert/strict";
import { signActorToken, verifyActorToken, bearerToken } from "./src/auth.ts";

const secret = "test-secret-123";
const actor = { kind: "human", id: "u:alice", display: "Alice" };

// Round-trip: a token signed with the secret verifies back to the same actor.
const tok = await signActorToken(actor, secret);
assert.deepEqual(await verifyActorToken(tok, secret), actor, "roundtrip");

// Wrong secret → unforgeable: cannot verify without the issuer secret.
assert.equal(await verifyActorToken(tok, "wrong-secret"), null, "wrong secret rejected");

// Tampered signature → rejected.
assert.equal(await verifyActorToken(tok.slice(0, -2) + "AA", secret), null, "tampered sig rejected");

// Swapped claim (attacker keeps a valid sig, swaps in a privileged actor) → rejected:
// the sig is over the original claim, so it won't match the new one.
const sig = tok.split(".")[1];
const evilClaim = Buffer.from(JSON.stringify({ kind: "system", id: "pillbox" })).toString("base64url");
assert.equal(await verifyActorToken(`${evilClaim}.${sig}`, secret), null, "swapped claim rejected");

// Malformed tokens → rejected, never throw.
for (const bad of ["", "garbage", "a.", ".b", "no-dot"]) {
  assert.equal(await verifyActorToken(bad, secret), null, `malformed rejected: ${JSON.stringify(bad)}`);
}

// Bearer extraction from an Authorization header.
assert.equal(bearerToken(new Request("https://x/", { headers: { authorization: "Bearer abc.def" } })), "abc.def");
assert.equal(bearerToken(new Request("https://x/")), null, "no header → null");

console.log("auth tests passed");
