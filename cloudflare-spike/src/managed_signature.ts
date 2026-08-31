import { managedCanonicalJson } from "./managed_contract.js";
import { decodeManagedEd25519PublicKey, fingerprintManagedEd25519PublicKeyBytes } from "./managed_signer.js";

const encoder = new TextEncoder();

/** Shared Ed25519 primitive; grant-specific modules retain their own validation and errors. */
export async function verifyManagedEd25519Signature(input: {
  readonly publicKeyMaterial: string;
  readonly signature: string;
  readonly claims: unknown;
}): Promise<{ readonly public_key_sha256: `sha256:${string}` }> {
  const publicKeyBytes = decodeManagedEd25519PublicKey(input.publicKeyMaterial);
  const key = await crypto.subtle.importKey(
    "raw",
    toArrayBuffer(publicKeyBytes),
    "Ed25519",
    false,
    ["verify"],
  );
  const valid = await crypto.subtle.verify(
    "Ed25519",
    key,
    toArrayBuffer(decodeBase64Url(input.signature)),
    toArrayBuffer(encoder.encode(managedCanonicalJson(input.claims))),
  );
  if (!valid) throw new Error("signature mismatch");
  return { public_key_sha256: await fingerprintManagedEd25519PublicKeyBytes(publicKeyBytes) };
}

function decodeBase64Url(value: string): Uint8Array {
  const padding = value.length % 4 === 0 ? "" : "=".repeat(4 - (value.length % 4));
  const decoded = atob(value.replaceAll("-", "+").replaceAll("_", "/") + padding);
  return Uint8Array.from(decoded, (character) => character.charCodeAt(0));
}

function toArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const buffer = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(buffer).set(bytes);
  return buffer;
}
