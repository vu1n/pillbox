const encoder = new TextEncoder();

export async function encryptOAuthMaterial(secret: string, material: unknown, aad = "pillbox-credential/v1"): Promise<string> {
  const key = await keyFor(secret);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const plaintext = encoder.encode(JSON.stringify(material));
  const ciphertext = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: encoder.encode(aad) }, key, plaintext));
  const packed = new Uint8Array(1 + iv.byteLength + ciphertext.byteLength);
  packed[0] = 1;
  packed.set(iv, 1);
  packed.set(ciphertext, 1 + iv.byteLength);
  return base64Url(packed);
}

export async function decryptOAuthMaterial<T>(secret: string, packed: string, aad = "pillbox-credential/v1"): Promise<T> {
  const bytes = decodeBase64Url(packed);
  if (bytes.byteLength <= 13 || bytes[0] !== 1) throw new Error("encrypted OAuth material version is unsupported");
  const key = await keyFor(secret);
  const plaintext = await crypto.subtle.decrypt({ name: "AES-GCM", iv: bytes.slice(1, 13), additionalData: encoder.encode(aad) }, key, bytes.slice(13));
  return JSON.parse(new TextDecoder().decode(plaintext)) as T;
}

async function keyFor(secret: string): Promise<CryptoKey> {
  if (!secret) throw new Error("credential encryption key is absent");
  const digest = await crypto.subtle.digest("SHA-256", encoder.encode(secret));
  return crypto.subtle.importKey("raw", digest, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
}

function base64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function decodeBase64Url(value: string): Uint8Array {
  const padding = value.length % 4 === 0 ? "" : "=".repeat(4 - (value.length % 4));
  const decoded = atob(value.replaceAll("-", "+").replaceAll("_", "/") + padding);
  return Uint8Array.from(decoded, (character) => character.charCodeAt(0));
}
