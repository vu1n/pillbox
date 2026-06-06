import { DurableObject } from "cloudflare:workers";
import type { Event, Payload } from "./contract.js";
import type { Env } from "./worker.js";

// Per-connection state persisted across hibernation. `cursor` = highest seq
// already delivered to this subscriber, so a woken DO knows where to resume.
interface WsAttach {
  role: "subscriber";
  cursor: number;
}

export class SessionGateway extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // Schema. seq is the PK and the sole authority; `at` + `payloadJson`
    // hold the rest of the envelope. Bounded per session (10GB cap).
    ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS log(
        seq         INTEGER PRIMARY KEY,
        at          TEXT NOT NULL,
        actorJson   TEXT,
        payloadJson TEXT NOT NULL
      );
    `);
  }

  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    switch (url.pathname) {
      case "/event":
        return this.handleEvent(req);
      case "/input":
        return this.handleInput(req);
      case "/subscribe":
        return this.handleSubscribe(req);
      default:
        return new Response("not found\n", { status: 404 });
    }
  }

  // ── append + seq authority ────────────────────────────────────────────
  // The single load-bearing primitive. transactionSync + single-threaded-
  // per-id => seq is strictly monotonic with no lock. Seq is derived from
  // storage (MAX), never an in-memory counter, so it survives eviction.
  private append(at: string, actor: unknown, payload: Payload): Event {
    const seq = this.ctx.storage.transactionSync(() => {
      const row = this.ctx.storage.sql
        .exec("SELECT COALESCE(MAX(seq), 0) AS m FROM log")
        .one() as { m: number };
      const next = row.m + 1;
      this.ctx.storage.sql.exec(
        "INSERT INTO log(seq, at, actorJson, payloadJson) VALUES (?, ?, ?, ?)",
        next,
        at,
        actor ? JSON.stringify(actor) : null,
        JSON.stringify(payload),
      );
      return next;
    });
    const ev: Event = { v: 1, seq, sessionId: this.idName(), at, payload };
    if (actor) ev.actor = actor as Event["actor"];
    this.fanout(ev);
    return ev;
  }

  private async handleEvent(req: Request): Promise<Response> {
    const body = (await req.json()) as Partial<Event>;
    if (!body.payload) return json({ error: "missing payload" }, 400);
    // STUB: actor is taken from the body, NOT authenticated. Milestone 1
    // stamps it from the connection. TODO: trust boundary.
    const ev = this.append(body.at ?? nowRfc3339(), body.actor, body.payload as Payload);
    return json({ seq: ev.seq, head: this.head() });
  }

  // Attributed input — the durable steer. Same append path => also fans out.
  private async handleInput(req: Request): Promise<Response> {
    const body = (await req.json()) as { text?: string; target?: string; mode?: string; actor?: unknown };
    const payload: Payload = {
      type: "input",
      text: body.text ?? "",
      target: (body.target as "agent" | "pty" | "exec") ?? "agent",
      mode: (body.mode as "live" | "turn") ?? "turn",
    };
    // STUB: no driver-token arbitration (milestone 4); no sandbox forward.
    // TODO: arbitrate target:pty via driver-token; forward to container PTY/exec.
    const ev = this.append(nowRfc3339(), body.actor, payload);
    return json({ seq: ev.seq, head: this.head() });
  }

  // ── Subscribe(from_seq) = replay then tail, over hibernatable WS ───────
  private async handleSubscribe(req: Request): Promise<Response> {
    if (req.headers.get("Upgrade") !== "websocket")
      return new Response("expected websocket\n", { status: 426 });

    const from = Number(new URL(req.url).searchParams.get("from") ?? "0");

    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);

    // Hibernation: acceptWebSocket (NOT server.accept) lets the DO evict
    // from memory while this connection stays open => cheap many-subscriber.
    this.ctx.acceptWebSocket(server, ["subscriber"]);

    // Replay everything from `from` synchronously, tracking the cursor.
    let cursor = from - 1;
    for (const ev of this.readFrom(from)) {
      server.send(JSON.stringify(ev));
      cursor = ev.seq;
    }
    // Persist resume point so a woken DO tails from the right place and a
    // reconnect-from-seq lands cleanly (deploy/migration story).
    const attach: WsAttach = { role: "subscriber", cursor };
    server.serializeAttachment(attach);

    return new Response(null, { status: 101, webSocket: client });
  }

  // Live tail: every append calls this. Sends only events past each
  // subscriber's cursor (handles the replay/tail boundary with no gap/dup).
  private fanout(ev: Event): void {
    for (const ws of this.ctx.getWebSockets("subscriber")) {
      const a = (ws.deserializeAttachment() as WsAttach | null) ?? { role: "subscriber", cursor: 0 };
      if (ev.seq > a.cursor) {
        try {
          ws.send(JSON.stringify(ev));
          ws.serializeAttachment({ ...a, cursor: ev.seq });
        } catch {
          // best-effort fan-out; a dead socket gets cleaned on close.
        }
      }
    }
  }

  // Hibernation handlers. Subscribers are read-only in the spike; messages
  // are accepted (e.g. {"from":N} re-replay) but the primary path is the URL.
  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    if (typeof message !== "string") return;
    try {
      const msg = JSON.parse(message);
      if (typeof msg.from === "number") {
        let cursor = msg.from - 1;
        for (const ev of this.readFrom(msg.from)) {
          ws.send(JSON.stringify(ev));
          cursor = ev.seq;
        }
        ws.serializeAttachment({ role: "subscriber", cursor });
      }
    } catch {
      /* ignore non-JSON pings */
    }
  }

  async webSocketClose(ws: WebSocket, code: number, _reason: string, _clean: boolean): Promise<void> {
    try {
      ws.close(code, "bye");
    } catch {
      /* already closed */
    }
  }

  // ── read helpers ──────────────────────────────────────────────────────
  private *readFrom(seq: number): Generator<Event> {
    const rows = this.ctx.storage.sql.exec(
      "SELECT seq, at, actorJson, payloadJson FROM log WHERE seq >= ? ORDER BY seq ASC",
      seq,
    );
    for (const r of rows as Iterable<{ seq: number; at: string; actorJson: string | null; payloadJson: string }>) {
      const ev: Event = {
        v: 1,
        seq: r.seq,
        sessionId: this.idName(),
        at: r.at,
        payload: JSON.parse(r.payloadJson) as Payload,
      };
      if (r.actorJson) ev.actor = JSON.parse(r.actorJson);
      yield ev;
    }
  }

  private head(): number {
    return (this.ctx.storage.sql.exec("SELECT COALESCE(MAX(seq), 0) AS m FROM log").one() as { m: number }).m;
  }

  // The DO doesn't natively know its idFromName key; in the spike we don't
  // need the literal sessionId in the envelope for the smoke, but keep the
  // field populated for contract parity. (Milestone 0 passes it in at spawn.)
  private idName(): string {
    return this.ctx.id.name ?? this.ctx.id.toString();
  }
}

function nowRfc3339(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
}
function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body) + "\n", {
    status,
    headers: { "content-type": "application/json" },
  });
}
