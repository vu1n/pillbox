// Sandbox round-trip smoke: prove the DO↔container hop + §0 round-trip.
// POST an /input command; a subscriber must see the input event (seq N) then the
// tool_call exec-output event (seq N+1) whose output carries the container stdout.
// Usage: node smoke-sandbox.mjs <base-url> <sessionId>   (Node >= 22)
const base = process.argv[2] ?? "http://127.0.0.1:8787";
const sid = process.argv[3] ?? `sbx-${Math.floor(Math.random() * 1e6)}`;
const agent = `/agents/session-gateway/${sid}`;
const MARK = `hello-from-container-${Math.floor(Math.random() * 1e6)}`;

function isEvent(ev) {
  return ev && typeof ev.seq === "number" && ev.payload; // skip cf_agent_* control frames
}

const seen = [];
const ws = new WebSocket(base.replace(/^http/, "ws") + `${agent}/subscribe?from=1`);
ws.addEventListener("message", (e) => {
  let ev;
  try {
    ev = JSON.parse(typeof e.data === "string" ? e.data : e.data.toString());
  } catch {
    return;
  }
  if (!isEvent(ev)) return;
  seen.push(ev);
  const p = ev.payload;
  console.log(`  <= seq=${ev.seq} type=${p.type}${p.output ? ` output=${JSON.stringify(p.output).slice(0, 80)}` : ""}`);
});

await new Promise((res) => ws.addEventListener("open", res));
await new Promise((r) => setTimeout(r, 300));

// Drive a command into the container. handleInput appends the input, awaits the
// exec hop, then appends the output — so by the time this resolves both events exist.
const r = await fetch(`${base}${agent}/input`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ text: `echo ${MARK}`, target: "exec", mode: "turn" }),
});
console.log("input ->", await r.json());
await new Promise((r) => setTimeout(r, 500));
ws.close();

const input = seen.find((e) => e.payload.type === "input");
const out = seen.find((e) => e.payload.type === "tool_call" && e.payload.name === "exec");
const ok =
  input &&
  out &&
  out.seq === input.seq + 1 &&
  out.payload.status === "completed" &&
  String(out.payload.output).includes(MARK);
console.log(
  ok
    ? `SANDBOX SMOKE PASS: input(seq ${input.seq}) → container exec → tool_call(seq ${out.seq}) carrying the container stdout, in order`
    : `SANDBOX SMOKE FAIL: input=${!!input} out=${JSON.stringify(out?.payload).slice(0, 120)}`,
);
process.exit(ok ? 0 : 1);
