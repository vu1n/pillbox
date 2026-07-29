export async function sha256Hex(value: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(value),
  );
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Cloudflare Sandbox IDs are DNS labels capped at 63 characters. Preserve
 * existing short runtime identities, but deterministically project longer or
 * reserved session identities without changing the authoritative SessionRef.
 */
export async function deriveSandboxRuntimeId(
  sessionId: string,
): Promise<string> {
  const reserved = new Set([
    "www",
    "api",
    "admin",
    "root",
    "system",
    "cloudflare",
    "workers",
  ]);
  if (
    sessionId.length > 0 &&
    sessionId.length <= 63 &&
    !sessionId.startsWith("-") &&
    !sessionId.endsWith("-") &&
    !reserved.has(sessionId.toLowerCase())
  ) {
    return sessionId;
  }
  return `pbx-${(await sha256Hex(sessionId)).slice(0, 59)}`;
}
