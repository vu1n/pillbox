import type { PillboxVerifiedSigner } from "./managed_contract.js";

/** Decode the exact raw 32-byte Ed25519 public key used by the verifier. */
export function decodeManagedEd25519PublicKey(value: string): Uint8Array {
  if (!value.startsWith("ed25519:")) throw new Error("public key must use ed25519: prefix");
  const bytes = decodeBase64Url(value.slice("ed25519:".length));
  if (bytes.byteLength !== 32) throw new Error("Ed25519 public key must be 32 bytes");
  return bytes;
}

export async function fingerprintManagedEd25519PublicKey(
  publicKeyMaterial: string,
): Promise<`sha256:${string}`> {
  return fingerprintManagedEd25519PublicKeyBytes(decodeManagedEd25519PublicKey(publicKeyMaterial));
}

export async function fingerprintManagedEd25519PublicKeyBytes(
  bytes: Uint8Array,
): Promise<`sha256:${string}`> {
  if (bytes.byteLength !== 32) throw new Error("Ed25519 public key must be 32 bytes");
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", toArrayBuffer(bytes)));
  return `sha256:${[...digest].map((byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

export function makeManagedVerifiedSigner(
  keyId: string,
  publicKeySha256: `sha256:${string}`,
): PillboxVerifiedSigner {
  if (keyId.length === 0) throw new Error("verified signer key id is empty");
  if (!/^sha256:[0-9a-f]{64}$/.test(publicKeySha256)) throw new Error("verified signer fingerprint is invalid");
  return { algorithm: "Ed25519", key_id: keyId, public_key_sha256: publicKeySha256 };
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
