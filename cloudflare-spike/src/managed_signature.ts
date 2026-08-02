import { managedCanonicalJson } from "./managed_contract.js";

const encoder = new TextEncoder();

/** Shared Ed25519 primitive; grant-specific modules retain their own validation and errors. */
export async function verifyManagedEd25519Signature(input: {
  readonly publicKeyMaterial: string;
  readonly signature: string;
  readonly claims: unknown;
}): Promise<void> {
  const key = await crypto.subtle.importKey(
    "raw",
    toArrayBuffer(decodeEd25519PublicKey(input.publicKeyMaterial)),
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
}

function decodeEd25519PublicKey(value: string): Uint8Array {
  if (!value.startsWith("ed25519:")) throw new Error("public key must use ed25519: prefix");
  const bytes = decodeBase64Url(value.slice("ed25519:".length));
  if (bytes.byteLength !== 32) throw new Error("Ed25519 public key must be 32 bytes");
  return bytes;
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
