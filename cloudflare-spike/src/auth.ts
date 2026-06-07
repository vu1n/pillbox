// A verifiable actor credential — the trust boundary for §0 attribution.
//
// The issuer (the control plane / orchestrator) signs an `Actor` claim with a
// shared secret; the DO verifies it server-side and stamps the *verified* actor
// onto the event. An in-sandbox agent (or any caller) can't forge one without the
// secret, so `actor` is attested, never self-reported — exactly the property
// session-event-log.md §Actor model requires for authz to key off it.
//
// Token = `base64url(claimJson).base64url(HMAC-SHA256(claimJson))`. HMAC (not a
// bearer→identity lookup) keeps the spike self-contained — no auth-provider
// round-trip — while still being unforgeable. A managed tier swaps this for
// control-plane-minted tokens bound to the principal; the verify-and-stamp shape
// here is unchanged.
import type { Actor } from "./contract.js";

const enc = new TextEncoder();
// The kinds a *token* may assert. `system` is deliberately excluded: it's the
// gateway's own identity (stamped directly as SYSTEM_ACTOR for lifecycle /
// arbitration / exec events), so a token that claims it would let any holder
// forge gateway-authored events. human/agent/service are the legitimate
// token-borne principals.
const TOKEN_KINDS = new Set(["human", "agent", "service"]);

async function hmacKey(secret: string): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"],
  );
}

function b64urlEncode(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function b64urlDecode(s: string): Uint8Array {
  const pad = s.length % 4 ? "=".repeat(4 - (s.length % 4)) : "";
  const b = atob(s.replace(/-/g, "+").replace(/_/g, "/") + pad);
  return Uint8Array.from(b, (c) => c.charCodeAt(0));
}

/** Sign an actor claim. Used by issuers (and the smoke test). */
export async function signActorToken(actor: Actor, secret: string): Promise<string> {
  const claim = b64urlEncode(enc.encode(JSON.stringify(actor)));
  const key = await hmacKey(secret);
  const sig = new Uint8Array(await crypto.subtle.sign("HMAC", key, enc.encode(claim)));
  return `${claim}.${b64urlEncode(sig)}`;
}

/**
 * Verify a token and return the attested `Actor`, or `null` if the signature is
 * invalid / the token is malformed / the claim isn't a well-formed Actor. The
 * caller treats `null` as unauthenticated. `crypto.subtle.verify` does the
 * constant-time compare, so this leaks no timing signal on the MAC.
 */
export async function verifyActorToken(token: string, secret: string): Promise<Actor | null> {
  const dot = token.indexOf(".");
  if (dot <= 0 || dot === token.length - 1) return null;
  const claimPart = token.slice(0, dot);
  const sigPart = token.slice(dot + 1);
  try {
    const key = await hmacKey(secret);
    const ok = await crypto.subtle.verify("HMAC", key, b64urlDecode(sigPart), enc.encode(claimPart));
    if (!ok) return null;
    const actor = JSON.parse(new TextDecoder().decode(b64urlDecode(claimPart))) as Actor;
    // A well-formed, token-assertable actor — `system` is rejected (see TOKEN_KINDS).
    if (!actor || typeof actor.id !== "string" || !TOKEN_KINDS.has(actor.kind)) return null;
    return actor;
  } catch {
    return null; // bad base64, bad JSON, or a crypto error — all unauthenticated
  }
}

/** Extract a bearer token from an `Authorization` header, or `null`. */
export function bearerToken(req: Request): string | null {
  const h = req.headers.get("authorization") ?? "";
  const m = /^Bearer\s+(.+)$/i.exec(h);
  return m ? m[1].trim() : null;
}
