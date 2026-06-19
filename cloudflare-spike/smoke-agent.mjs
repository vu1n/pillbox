// Consume-path falsifier (docs/managed-tier.md §Consume path): drive ONE real
// opencode turn through the DO↔container hop and assert the §0 stream. A
// subscriber must see, in seq order:
//   input(human) → message_start → message_delta+ → message_end → attention_required
// with the agent events stamped actor.id === "a:opencode" (gateway-stamped, never
// self-reported by opencode-in-the-container). Proves opencode-in-Sandbox + the
// SSE tail + the TS OpencodeMapper + §0 append + subscribe, end-to-end, and
// exercises Open Question #2 (the DO holds a long-lived SSE for the whole turn).
//
// Needs the CONTAINER deploy (wrangler.container.toml; Workers Paid) + a provider
// key secret — the agent turn won't run on the free/§0-only deploy. arm64 hosts
// can't boot the amd64 container locally, so run this against a real CF deploy.
//
// Usage: node smoke-agent.mjs <base-url> [sessionId] [model]
//   ACTOR_TOKEN_SECRET (env) must match the deploy's secret.
import { signActorToken } from "./src/auth.ts";

const base = process.argv[2] ?? "http://127.0.0.1:8787";
const sid = process.argv[3] ?? `agent-${Math.floor(Math.random() * 1e6)}`;
const model = process.argv[4]; // optional; else the DO's OPENCODE_MODEL / default
const secret = process.env.ACTOR_TOKEN_SECRET ?? "dev-secret-for-local-only";
const agent = `/agents/session-gateway/${sid}`;

const token = await signActorToken({ kind: "human", id: "u:smoke", display: "smoke" }, secret);

function isEvent(ev) {
  return ev && typeof ev.seq === "number" && ev.payload; // skip cf_agent_* control frames
}

const seen = [];
const ws = new WebSocket(
  base.replace(/^http/, "ws") + `${agent}/subscribe?from=1&token=${encodeURIComponent(token)}`,
);
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
  const a = ev.actor ? `${ev.actor.kind}:${ev.actor.id}` : "-";
  console.log(
    `  <= seq=${ev.seq} actor=${a} type=${p.type}${p.text ? ` text=${JSON.stringify(p.text).slice(0, 40)}` : ""}`,
  );
});

await new Promise((res) => ws.addEventListener("open", res));
await new Promise((r) => setTimeout(r, 300));

// Drive one agent turn. handleInput awaits the whole turn (driveAgent tails the
// SSE to idle), so by the time this fetch resolves every §0 event for the turn exists.
const r = await fetch(`${base}${agent}/input`, {
  method: "POST",
  headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
  body: JSON.stringify({
    text: "Reply with exactly: OK. Do not use any tools.",
    target: "agent",
    ...(model ? { model } : {}),
  }),
});
console.log("input ->", r.status, (await r.text()).trim());
await new Promise((r) => setTimeout(r, 800)); // let the final fan-out settle
ws.close();

// Assert the ordered §0 turn shape.
const byType = (t) => seen.filter((e) => e.payload.type === t);
const input = seen.find((e) => e.payload.type === "input");
const start = byType("message_start")[0];
const deltas = byType("message_delta");
const end = byType("message_end")[0];
// The turn-done signal (needs_input) — NOT a gateway error (error_stalled, system-stamped).
const attn = byType("attention_required").find((e) => e.payload.reason === "needs_input");

const agentStamped = (e) => !!e && !!e.actor && e.actor.kind === "agent" && e.actor.id === "a:opencode";
const ordered =
  !!input &&
  !!start &&
  !!end &&
  !!attn &&
  input.seq < start.seq &&
  deltas.length > 0 &&
  deltas.every((d) => d.seq > start.seq && d.seq < end.seq) &&
  end.seq < attn.seq;
const stamped =
  agentStamped(start) && deltas.every(agentStamped) && agentStamped(end) && agentStamped(attn);

const ok = ordered && stamped;
console.log(
  ok
    ? `AGENT SMOKE PASS: input(seq ${input.seq}) → message_start(${start.seq}) → ${deltas.length} delta(s) → message_end(${end.seq}) → attention_required(${attn.seq}), all actor=a:opencode`
    : `AGENT SMOKE FAIL: input=${!!input} start=${!!start} deltas=${deltas.length} end=${!!end} attn=${!!attn} ordered=${ordered} stamped=${stamped}`,
);
process.exit(ok ? 0 : 1);
