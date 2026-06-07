import { Agent, type Connection, type ConnectionContext, type WSMessage } from "agents";
import { getSandbox } from "@cloudflare/sandbox";
import type { Event, Payload } from "./contract.js";
import type { Env } from "./worker.js";

// Per-connection state (the subscriber's replay cursor), persisted across
// hibernation via `connection.setState` — the Agents-SDK replacement for raw
// `serializeAttachment`. NOTE: this is per-connection state, NOT the global
// `this.setState`. We deliberately NEVER call global `this.setState`: it is
// last-write-wins whole-object sync, structurally incompatible with an ordered
// append-only log keyed by monotonic `seq` (it'd lose ordering, replay, and
// per-event actor attribution). The §0 log lives in `this.ctx.storage.sql`; the
// cursor lives here. (This is the exact split GSV runs in production.)
type WsState = { role: "subscriber"; cursor: number };

export class SessionGateway extends Agent<Env> {
  // Agents-SDK lifecycle hook (replaces the constructor's table create). The §0
  // log is our own SQLite table, not Agent state — full control, no sync.
  onStart(): void {
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS log(
        seq         INTEGER PRIMARY KEY,
        at          TEXT NOT NULL,
        actorJson   TEXT,
        payloadJson TEXT NOT NULL
      );
    `);
  }

  // ── append + seq authority ────────────────────────────────────────────
  // UNCHANGED from the raw-DO spike: transactionSync + single-threaded-per-id =>
  // seq is strictly monotonic with no lock. Seq is derived from storage (MAX),
  // never an in-memory counter, so it survives eviction. This is a DO primitive
  // the Agent class inherits, so adopting `Agent` costs the keystone nothing.
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
    // `this.name` is the Agent instance name (the sessionId) — resolves the
    // raw-DO spike's `ctx.id.name` readback caveat for free.
    const ev: Event = { v: 1, seq, sessionId: this.name, at, payload };
    if (actor) ev.actor = actor as Event["actor"];
    this.fanout(ev);
    return ev;
  }

  // HTTP: POST /event and /input both land here (routeAgentRequest forwards the
  // request; WS upgrades go to onConnect instead). Dispatch on the trailing path.
  async onRequest(req: Request): Promise<Response> {
    const path = new URL(req.url).pathname;
    if (path.endsWith("/event")) return this.handleEvent(req);
    if (path.endsWith("/input")) return this.handleInput(req);
    return new Response("not found\n", { status: 404 });
  }

  private async handleEvent(req: Request): Promise<Response> {
    const body = (await req.json()) as Partial<Event>;
    if (!body.payload) return json({ error: "missing payload" }, 400);
    // STUB: actor taken from the body, NOT authenticated. Milestone 1 stamps it
    // from the connection in onConnect (the trust boundary). TODO.
    const ev = this.append(body.at ?? nowRfc3339(), body.actor, body.payload as Payload);
    return json({ seq: ev.seq, head: this.head() });
  }

  // Attributed input — the durable steer, then driven into the container. The
  // input is appended (seq N, fans out), the call crosses the DO↔container hop
  // (the one unmeasured managed-tier risk), and the result is appended as a §0
  // event (seq N+1) so a subscriber sees the round-trip: input → output.
  private async handleInput(req: Request): Promise<Response> {
    const body = (await req.json()) as { text?: string; target?: string; mode?: string; actor?: unknown };
    const payload: Payload = {
      type: "input",
      text: body.text ?? "",
      target: (body.target as "agent" | "pty" | "exec") ?? "exec",
      mode: (body.mode as "live" | "turn") ?? "turn",
    };
    // STUB: no driver-token arbitration (milestone 4).
    const inEv = this.append(nowRfc3339(), body.actor, payload);
    await this.driveSandbox(inEv.seq, body.text ?? "");
    return json({ seq: inEv.seq, head: this.head() });
  }

  // The DO↔container hop. One container per session (addressed by the same
  // sessionId). Cycle-1: run the input as a command and append its output as a
  // §0 `tool_call` event. (The streaming-agent producer — the in-container
  // tailer POSTing /event back with seq=0 — is the next sub-slice; this proves
  // the hop + the round-trip first.) `getSandbox` is the Sandbox-SDK handle to
  // the sibling container DO.
  private async driveSandbox(inputSeq: number, cmd: string): Promise<void> {
    const opId = `exec-${inputSeq}`;
    const sandbox = getSandbox(this.env.Sandbox, this.name);
    // Cold-start: the container DO boots on first use and `exec` throws a
    // transient "Container is starting" until it's up. Retry that case with a
    // short backoff; surface any non-transient error as-is.
    let lastErr = "";
    for (let attempt = 0; attempt < 15; attempt++) {
      try {
        const res = await sandbox.exec(cmd);
        const out = [res.stdout, res.stderr].filter(Boolean).join("\n");
        this.append(nowRfc3339(), undefined, {
          type: "tool_call",
          toolCallId: opId,
          name: "exec",
          status: res.success ? "completed" : "failed",
          output: out,
        });
        return;
      } catch (e) {
        lastErr = String(e);
        if (!/starting|not ready/i.test(lastErr)) break; // non-transient → stop
        await new Promise((r) => setTimeout(r, 1000)); // container cold-start backoff
      }
    }
    this.append(nowRfc3339(), undefined, {
      type: "tool_call",
      toolCallId: opId,
      name: "exec",
      status: "error",
      output: lastErr,
    });
  }

  // ── Subscribe(from_seq) = replay then tail ────────────────────────────
  // The Agents SDK accepts + hibernates the WebSocket for us (no WebSocketPair /
  // acceptWebSocket / 101 plumbing). We just replay from `from` and record the
  // cursor on the connection.
  async onConnect(connection: Connection<WsState>, ctx: ConnectionContext): Promise<void> {
    const from = Number(new URL(ctx.request.url).searchParams.get("from") ?? "0");
    // MILESTONE 1: stamp `actor` here from the authenticated ctx.request — the
    // trust boundary the raw-DO spike couldn't reach. TODO.
    let cursor = from - 1;
    for (const ev of this.readFrom(from)) {
      connection.send(JSON.stringify(ev));
      cursor = ev.seq;
    }
    connection.setState({ role: "subscriber", cursor });
  }

  // Optional re-replay on a {"from":N} client message (reconnect-from-seq).
  async onMessage(connection: Connection<WsState>, message: WSMessage): Promise<void> {
    if (typeof message !== "string") return;
    try {
      const msg = JSON.parse(message);
      if (typeof msg.from === "number") {
        let cursor = msg.from - 1;
        for (const ev of this.readFrom(msg.from)) {
          connection.send(JSON.stringify(ev));
          cursor = ev.seq;
        }
        connection.setState({ role: "subscriber", cursor });
      }
    } catch {
      /* ignore non-JSON pings */
    }
  }

  // Live tail: every append fans out to each subscriber past its cursor (closes
  // the replay/tail boundary with no gap/dup). `getConnections()` replaces the
  // raw `getWebSockets(tag)`.
  private fanout(ev: Event): void {
    for (const conn of this.getConnections<WsState>()) {
      const a = conn.state ?? { role: "subscriber" as const, cursor: 0 };
      if (ev.seq > a.cursor) {
        try {
          conn.send(JSON.stringify(ev));
          conn.setState({ ...a, cursor: ev.seq });
        } catch {
          // best-effort fan-out; a dead socket is cleaned on close.
        }
      }
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
        sessionId: this.name,
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
