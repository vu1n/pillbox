// §0 replay/tail RACE stress (docs/managed-tier.md: "replay/tail race needs a
// stress-smoke before trusting"). The DO is the sole seq authority and the
// subscribe surface must, under concurrent load, deliver every subscriber a
// gap-free / dup-free / strictly-ordered run from its `from` cursor to head —
// including subscribers that connect MID-append-burst (the dangerous case: an
// append committing between the SQL replay snapshot and live-tail registration
// must be neither missed nor double-sent).
//
// Pure §0: drives load via /annotation (authenticated, NOT driver-gated, one
// append each, no container) so this exercises the sequencer + fan-out + the
// per-connection replay cursor in isolation — no container cold-start.
//
// Usage: node stress-smoke.mjs <base-url> [sessionId] [subscribers=24] [appends=250]
//   ACTOR_TOKEN_SECRET (env) must match the deploy's secret.
import { signActorToken } from "./src/auth.ts";

const base = process.argv[2] ?? "http://127.0.0.1:8787";
const sid = process.argv[3] ?? `stress-${Math.floor(Math.random() * 1e6)}`;
const K = Number(process.argv[4] ?? 24);
const M = Number(process.argv[5] ?? 250);
const secret = process.env.ACTOR_TOKEN_SECRET ?? "dev-secret-for-local-only";
const agent = `${base}/agents/session-gateway/${sid}`;
const wsAgent = agent.replace(/^http/, "ws");

const actors = await Promise.all(
  [0, 1, 2, 3].map((i) => signActorToken({ kind: "human", id: `u:s${i}`, display: `s${i}` }, secret)),
);
const tok0 = actors[0];

const isEvent = (ev) => ev && typeof ev.seq === "number" && ev.payload; // skip cf_agent_* frames

// A subscriber that records arrival order, duplicates, and the payload per seq.
function subscribe(from, label) {
  const got = new Map(); // seq -> payloadJson
  let lastSeq = 0;
  let outOfOrder = 0;
  let dups = 0;
  const ws = new WebSocket(`${wsAgent}/subscribe?from=${from}&token=${encodeURIComponent(tok0)}`);
  ws.addEventListener("message", (e) => {
    let ev;
    try {
      ev = JSON.parse(typeof e.data === "string" ? e.data : e.data.toString());
    } catch {
      return;
    }
    if (!isEvent(ev)) return;
    if (got.has(ev.seq)) dups++;
    if (ev.seq <= lastSeq) outOfOrder++; // frames must arrive strictly increasing
    lastSeq = ev.seq;
    got.set(ev.seq, JSON.stringify(ev.payload));
  });
  const ready = new Promise((r) => ws.addEventListener("open", () => r()));
  return { from, label, got, stats: () => ({ outOfOrder, dups }), ready, close: () => ws.close() };
}

async function annotate(i) {
  const t = actors[i % actors.length];
  const r = await fetch(`${agent}/annotation`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${t}` },
    body: JSON.stringify({ text: `a${i}` }),
  });
  if (!r.ok) throw new Error(`annotation ${i} -> ${r.status}`);
  return (await r.json()).seq;
}

console.log(`stress: sid=${sid} subscribers=${K} appends=${M}`);

// Cohort A: connected before any append (replay is empty, pure tail).
const subs = [];
for (let i = 0; i < Math.ceil(K / 2); i++) subs.push(subscribe(1, `pre#${i}`));
await Promise.all(subs.map((s) => s.ready));

// Append burst with bounded concurrency. Cohort B (late subscribers) connects
// mid-burst — the replay/tail boundary stress — at from=1 and from=a-live-offset.
const returned = [];
let launchedB = false;
let idx = 0;
async function worker() {
  while (idx < M) {
    const i = idx++;
    returned.push(await annotate(i));
    if (!launchedB && i >= Math.floor(M * 0.4)) {
      launchedB = true;
      for (let j = 0; j < Math.floor(K / 2); j++) subs.push(subscribe(1, `mid#${j}`));
      subs.push(subscribe(Math.max(1, returned.length - 5), `offset`)); // replay-from-offset under load
    }
  }
}
await Promise.all(Array.from({ length: 30 }, worker));

const head = Math.max(...returned);
await new Promise((r) => setTimeout(r, 2500)); // let fan-out + late-subscriber catch-up settle
subs.forEach((s) => s.close());

// Assertions. A clean run appends seq 1..M with no system events, so head===M and
// every from=1 subscriber must hold exactly {1..head}; a from=F subscriber {F..head}.
let pass = true;
const fail = (msg) => {
  pass = false;
  console.log(`  FAIL ${msg}`);
};
if (head !== M) fail(`head=${head} != appends=${M} (unexpected extra/missing §0 events)`);
if (new Set(returned).size !== M) fail(`append seqs not unique: ${M - new Set(returned).size} collision(s)`);

let perfect = 0;
for (const s of subs) {
  const { outOfOrder, dups } = s.stats();
  const missing = [];
  for (let q = s.from; q <= head; q++) if (!s.got.has(q)) missing.push(q);
  const extra = [...s.got.keys()].filter((q) => q < s.from || q > head);
  const ok = missing.length === 0 && extra.length === 0 && dups === 0 && outOfOrder === 0;
  if (ok) perfect++;
  else
    fail(
      `${s.label} from=${s.from}: missing=${missing.length}${missing.length ? `[${missing.slice(0, 5)}…]` : ""} extra=${extra.length} dups=${dups} outOfOrder=${outOfOrder}`,
    );
}

// Cross-subscriber agreement: the payload at each seq must be identical for all
// (the DO is the single source of truth; divergence = a fan-out corruption).
const ref = subs.find((s) => s.from === 1)?.got;
if (ref) {
  for (let q = 1; q <= head; q++) {
    const want = ref.get(q);
    for (const s of subs) {
      if (s.from <= q && s.got.get(q) !== want) {
        fail(`seq ${q} payload diverges for ${s.label}`);
        break;
      }
    }
  }
}

console.log(
  pass
    ? `STRESS PASS: head=${head}, ${subs.length} subscribers each saw a gap-free/dup-free/ordered range to head; payloads agree. (${perfect}/${subs.length} perfect)`
    : `STRESS FAIL (see above): head=${head}, ${perfect}/${subs.length} subscribers clean`,
);
process.exit(pass ? 0 : 1);
