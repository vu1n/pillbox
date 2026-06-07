// Live driver-arbitration smoke: authZ over authN. One driver at a time —
// actor A claims the slot on first /input, drives again freely, actor B is 409'd
// until it opts to steal, and the current driver can release. Each transition
// lands a `driver_changed` §0 event (actor = system). Run against `wrangler dev`:
//   node smoke-driver.mjs [base-url]
import { signActorToken } from "./src/auth.ts";

const base = process.argv[2] ?? "http://127.0.0.1:8788";
const secret = process.env.ACTOR_TOKEN_SECRET ?? "dev-secret-for-local-only";
const sid = "drv-" + Date.now().toString(36);
const agent = `/agents/session-gateway/${sid}`;

const tokA = await signActorToken({ kind: "human", id: "u:alice", display: "Alice" }, secret);
const tokB = await signActorToken({ kind: "human", id: "u:bob", display: "Bob" }, secret);

const post = (path, token, body) =>
  fetch(`${base}${agent}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
    body: JSON.stringify(body ?? {}),
  });
const input = (token, body) => post("/input", token, { text: "echo hi", ...body });

// 1. A's first /input claims the driver (→ driver_changed granted).
const a1 = await input(tokA);
console.log("A /input (claim) →", a1.status, "(expect 200)");

// 2. A drives again — already the driver, no new driver_changed.
const a2 = await input(tokA);
console.log("A /input (again) →", a2.status, "(expect 200)");

// 3. B's /input is rejected — A holds the slot.
const b1 = await input(tokB);
const b1body = await b1.json().catch(() => ({}));
console.log("B /input (no steal) →", b1.status, "driver:", JSON.stringify(b1body.driver), "(expect 409)");

// 4. B steals via mode:"steal" (→ driver_changed stolen).
const b2 = await input(tokB, { mode: "steal" });
console.log("B /input (steal) →", b2.status, "(expect 200)");

// 5. A is now NOT the driver → 409.
const a3 = await input(tokA);
console.log("A /input (after steal) →", a3.status, "(expect 409)");

// 6. B releases the slot.
const rel = await post("/driver/release", tokB);
console.log("B /driver/release →", rel.status, "(expect 200)");

// 7. A reclaims (slot is free → granted again).
const a4 = await input(tokA);
console.log("A /input (reclaim) →", a4.status, "(expect 200)");

// Read back the §0 log over the open subscribe socket and inspect the
// driver_changed sequence.
const ws = new WebSocket(base.replace(/^http/, "ws") + `${agent}/subscribe?from=1`);
const events = [];
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
      events.push(e);
      clearTimeout(setTimeout(res, 600)); // settle window after last event
    }
  };
});
ws.close();

const driverChanges = events.filter((e) => e.payload.type === "driver_changed").map((e) => ({
  mode: e.payload.mode,
  from: e.payload.from?.id,
  to: e.payload.to?.id,
  actorKind: e.actor?.kind,
}));
console.log("driver_changed events →", JSON.stringify(driverChanges, null, 2));

const ok =
  a1.status === 200 &&
  a2.status === 200 &&
  b1.status === 409 &&
  b1body.driver?.id === "u:alice" &&
  b2.status === 200 &&
  a3.status === 409 &&
  rel.status === 200 &&
  a4.status === 200 &&
  // expect exactly: granted(alice), stolen(alice→bob), released(bob), granted(alice)
  driverChanges.length === 4 &&
  driverChanges[0].mode === "granted" &&
  driverChanges[0].to === "u:alice" &&
  driverChanges[1].mode === "stolen" &&
  driverChanges[1].from === "u:alice" &&
  driverChanges[1].to === "u:bob" &&
  driverChanges[2].mode === "released" &&
  driverChanges[2].from === "u:bob" &&
  driverChanges[3].mode === "granted" &&
  driverChanges[3].to === "u:alice" &&
  driverChanges.every((d) => d.actorKind === "system");

console.log(ok ? "DRIVER SMOKE PASS ✓ (one driver; claim/steal/release arbitrated)" : "DRIVER SMOKE FAIL ✗");
process.exit(ok ? 0 : 1);
