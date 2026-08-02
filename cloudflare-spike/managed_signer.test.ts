import assert from "node:assert/strict";
import { test } from "node:test";
import { fingerprintManagedEd25519PublicKey, makeManagedVerifiedSigner } from "./src/managed_signer.ts";

function base64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

test("verified signer fingerprints the exact raw Ed25519 public key", async () => {
  const rawKey = Uint8Array.from({ length: 32 }, (_, index) => index);
  const fingerprint = await fingerprintManagedEd25519PublicKey(`ed25519:${base64Url(rawKey)}`);
  assert.equal(fingerprint, "sha256:630dcd2966c4336691125448bbb25b4ff412a49c732db2c8abc1b8581bd710dd");
  assert.deepEqual(makeManagedVerifiedSigner("huddles-key-1", fingerprint), {
    algorithm: "Ed25519",
    key_id: "huddles-key-1",
    public_key_sha256: fingerprint,
  });
});

test("verified signer identity rejects malformed fingerprints", () => {
  assert.throws(() => makeManagedVerifiedSigner("huddles-key-1", "sha256:bad" as `sha256:${string}`), /fingerprint/);
});
