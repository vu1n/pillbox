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
const ok =
  noTok.status === 401 &&
  withTok.status === 200 &&
  stamped?.kind === "human" &&
  stamped?.id === "u:alice";
console.log(ok ? "AUTH SMOKE PASS ✓ (body-claimed actor ignored; token actor stamped)" : "AUTH SMOKE FAIL ✗");
process.exit(ok ? 0 : 1);
