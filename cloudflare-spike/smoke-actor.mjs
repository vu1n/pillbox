// Live trust-boundary smoke: actor is stamped from the verified token, never the
// request body, and writes require a valid token. Run against `wrangler dev`:
//   node smoke-actor.mjs [base-url]
import { signActorToken } from "./src/auth.ts";

const base = process.argv[2] ?? "http://127.0.0.1:8799";
const secret = process.env.ACTOR_TOKEN_SECRET ?? "dev-secret-for-local-only";
const sid = "auth-" + Date.now().toString(36);
const agent = `/agents/session-gateway/${sid}`;

const token = await signActorToken({ kind: "human", id: "u:alice", display: "Alice" }, secret);
const evtBody = (extra = {}) =>
  JSON.stringify({ payload: { type: "message_end", messageId: "m1" }, ...extra });

// 1. No token → 401 (writes require attestation).
const noTok = await fetch(`${base}${agent}/event`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: evtBody(),
});
console.log("no-token /event →", noTok.status, "(expect 401)");

// 2. Valid token, but the BODY claims a different (privileged) actor → 200, and
//    the stamped actor must be the token's, proving the body claim is ignored.
const withTok = await fetch(`${base}${agent}/event`, {
  method: "POST",
  headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
  body: evtBody({ actor: { kind: "system", id: "pillbox" } }),
});
console.log("token /event →", withTok.status, "(expect 200)");

// 3. Read it back over the (open) subscribe socket.
const ws = new WebSocket(base.replace(/^http/, "ws") + `${agent}/subscribe?from=1`);
const got = [];
await new Promise((res) => {
  const t = setTimeout(res, 3000);
  ws.onmessage = (m) => {
    let e;
    try {
      e = JSON.parse(m.data);
    } catch {
      return;
    }
    if (e && e.payload && typeof e.seq === "number") {
      got.push(e);
      clearTimeout(t);
      res();
    }
  };
});
ws.close();

const stamped = got[0]?.actor;
console.log("stamped actor →", JSON.stringify(stamped));

// 4. A token claiming `system` → 401 (system is gateway-only, not token-borne).
const sysTok = await signActorToken({ kind: "system", id: "pillbox" }, secret);
const sysRes = await fetch(`${base}${agent}/event`, {
  method: "POST",
  headers: { "content-type": "application/json", authorization: `Bearer ${sysTok}` },
  body: evtBody(),
});
console.log("system-token /event →", sysRes.status, "(expect 401)");

// 5. A control payload (driver_changed) on /event → 403 (forging the wrong door).
const forgeRes = await fetch(`${base}${agent}/event`, {
  method: "POST",
  headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
  body: JSON.stringify({ payload: { type: "driver_changed", to: { kind: "human", id: "u:alice" }, mode: "granted" } }),
});
console.log("forged driver_changed /event →", forgeRes.status, "(expect 403)");

const ok =
  noTok.status === 401 &&
  withTok.status === 200 &&
  stamped?.kind === "human" &&
  stamped?.id === "u:alice" &&
  sysRes.status === 401 &&
  forgeRes.status === 403;
console.log(ok ? "AUTH SMOKE PASS ✓ (body actor ignored; system-token + control-payload forgery rejected)" : "AUTH SMOKE FAIL ✗");
process.exit(ok ? 0 : 1);
