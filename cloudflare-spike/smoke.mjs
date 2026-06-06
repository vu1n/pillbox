// Tiny §0 subscriber: connects, prints each Event in seq order.
// Usage: node smoke.mjs <base-url> <sessionId>   (Node >= 22 for built-in fetch + WebSocket)
const base = process.argv[2] ?? "http://127.0.0.1:8787";
const sid = process.argv[3] ?? "spike-sess-1";

// Agents-SDK route convention: /agents/<kebab-class>/<instance-name>/*
// routeAgentRequest forwards HTTP → onRequest, WS upgrade → onConnect.
const agent = `/agents/session-gateway/${sid}`;
const wsUrl = base.replace(/^http/, "ws") + `${agent}/subscribe?from=1`;

async function post(op, body) {
  const r = await fetch(`${base}${agent}/${op}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  return r.json();
}

// 1. Append three events BEFORE subscribing (proves replay-from-seq).
console.log("append 1:", await post("event", { payload: { type: "message_delta", messageId: "m1", text: "hello" } }));
console.log("append 2:", await post("event", { payload: { type: "tool_call", toolCallId: "t1", name: "Bash", status: "running" } }));
console.log("input  3:", await post("input", { text: "run the tests", target: "agent", mode: "turn" }));

// 2. Subscribe from seq 1 => must REPLAY 1,2,3 then TAIL the next append.
const seen = [];
const ws = new WebSocket(wsUrl);
ws.addEventListener("message", (e) => {
  const ev = JSON.parse(e.data);
  seen.push(ev.seq);
  console.log(`  <= seq=${ev.seq} type=${ev.payload.type}`);
});

await new Promise((res) => ws.addEventListener("open", res));
await new Promise((r) => setTimeout(r, 300)); // let replay flush

// 3. Append a 4th AFTER subscribe => proves live tail to the open WS.
console.log("append 4:", await post("event", { payload: { type: "message_end", messageId: "m1" } }));
await new Promise((r) => setTimeout(r, 300));

ws.close();
const ok = JSON.stringify(seen) === JSON.stringify([1, 2, 3, 4]);
console.log(ok ? "SMOKE PASS: replay 1-3 then tail 4, in seq order" : `SMOKE FAIL: saw ${JSON.stringify(seen)}`);
process.exit(ok ? 0 : 1);
