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
  // EventLog::append — resident-sequencer placement (1:1 with the local
  // src/events/log.rs::SessionLog::append, which is the co-located single-writer
  // placement). The log is the seq authority: this method ASSIGNS seq from
  // storage (MAX(seq)+1) and never reads a producer-supplied seq — a client that
  // POSTs `seq=42` is ignored exactly as SessionLog::append overwrites the
  // producer's seq. transactionSync + single-threaded-per-id => seq is strictly
  // monotonic with no lock. Seq is derived from storage (MAX), never an in-memory
  // counter, so it survives eviction (the DO analogue of SessionLog recovering
  // last_seq from log.jsonl on open). This is a DO primitive the Agent class
  // inherits, so adopting `Agent` costs the keystone nothing.
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

  // The /event producer path → EventLog::append. A producer (the in-sandbox §0
  // tailer, a host process) submits an Event; this is the resident-sequencer
  // analogue of a co-located producer calling SessionLog::append. Note the
  // producer-supplied `body.seq` is DISCARDED — `append` stamps the authority's
  // seq, matching SessionLog::append's overwrite-the-producer's-seq rule.
  //
  // TODO(actor-stamping — the next slice, NOT this task): authenticate + stamp
  // `actor` at this trust boundary. Today `body.actor` is taken from the request
  // body UNAUTHENTICATED — any caller can claim any actor, so actor is not yet an
  // authz signal (exactly the `emitter` tag's caveat in session-event-log.md).
  // The fix per session-event-log.md §Actor model:
  //   1. The actor CLAIM arrives as a verifiable credential on the request — a
  //      bearer/signed token (per-connection on WS, per-request header on HTTP),
  //      NOT a body field. Managed tier: minted by the control plane and bound to
  //      the principal (human user / agent / service).
  //   2. Verify it server-side here (and in onConnect), derive the Actor from the
  //      verified claim, and stamp it into the appended event.
  //   3. IGNORE any body-supplied `actor` entirely (treat as spoofed). Authz —
  //      who may drive/approve/join — then keys off the stamped, trusted actor.
  // Until then `body.actor` is a stub for shape only.
  private async handleEvent(req: Request): Promise<Response> {
    const body = (await req.json()) as Partial<Event>;
    if (!body.payload) return json({ error: "missing payload" }, 400);
    // STUB: actor taken from the body, NOT authenticated. See the TODO above.
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
    // Drive the container only when one is bound (the container config). On the
    // free/§0-only deploy there's no Sandbox binding, so `/input` is append-only
    // — the attributed-input §0 path still works, just without the exec hop.
    if (this.env.Sandbox) {
      await this.driveSandbox(this.env.Sandbox, inEv.seq, body.text ?? "");
    }
    return json({ seq: inEv.seq, head: this.head() });
  }

  // The DO↔container hop. One container per session (addressed by the same
  // sessionId). Cycle-1: run the input as a command and append its output as a
  // §0 `tool_call` event. (The streaming-agent producer — the in-container
  // tailer POSTing /event back with seq=0 — is the next sub-slice; this proves
  // the hop + the round-trip first.) `getSandbox` is the Sandbox-SDK handle to
  // the sibling container DO.
  // Takes the binding explicitly (non-optional) so the precondition "only with a
  // container bound" is in the type, not just the caller's guard.
  private async driveSandbox(
    sandboxNs: NonNullable<Env["Sandbox"]>,
    inputSeq: number,
    cmd: string,
  ): Promise<void> {
    const opId = `exec-${inputSeq}`;
    const sandbox = getSandbox(sandboxNs, this.name);
    // Cold-start: the container DO boots on first use and `exec` throws a
    // transient "Container is starting" until it's up. Retry that case with a
    // short backoff; surface any non-transient error as-is.
    let lastErr = "";
    for (let attempt = 0; attempt < 15; attempt++) {
      try {
        const res = await sandbox.exec(cmd);
        const out = [res.stdout, res.stderr].filter(Boolean).join("\n");
        this.appendExec(opId, res.success ? "completed" : "failed", out);
        return;
      } catch (e) {
        lastErr = String(e);
        if (!/starting|not ready/i.test(lastErr)) break; // non-transient → stop
        await new Promise((r) => setTimeout(r, 1000)); // container cold-start backoff
      }
    }
    this.appendExec(opId, "error", lastErr);
  }

  // Append the container exec's outcome as a §0 tool_call event (fans out to subscribers).
  private appendExec(opId: string, status: string, output: string): void {
    this.append(nowRfc3339(), undefined, {
      type: "tool_call",
      toolCallId: opId,
      name: "exec",
      status,
      output,
    });
  }

  // ── Subscribe(from_seq) = replay then tail ────────────────────────────
  // EventLog::subscribe — resident-sequencer placement (1:1 with
  // src/events/log.rs::SessionLog::subscribe: replay seq>=from, then tail live
  // appends). Replay is readFrom(from) (== SessionLog::read_from); the live tail
  // is `fanout` pushing each new append past this connection's cursor (the DO's
  // WS fan-out replaces SessionLog's notify-on-the-file bus). The Agents SDK
  // accepts + hibernates the WebSocket for us (no WebSocketPair / acceptWebSocket
  // / 101 plumbing). We replay from `from` and record the cursor on the connection.
  //
  // TODO(actor-stamping — the next slice, NOT this task): this is the WS half of
  // the same trust boundary as handleEvent. The connection's actor must be
  // derived from a verified credential on `ctx.request` (a bearer/signed token on
  // the upgrade, e.g. a query/header/subprotocol carrying the control-plane-minted
  // token), verified here, and bound to the connection (e.g. in WsState) so any
  // /input or /event arriving on this socket is stamped with the connection's
  // authenticated actor — never a body-supplied one. See handleEvent's TODO.
  async onConnect(connection: Connection<WsState>, ctx: ConnectionContext): Promise<void> {
    const from = Number(new URL(ctx.request.url).searchParams.get("from") ?? "0");
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
  // EventLog::read_from — replay every durable event with seq>=from (1:1 with
  // src/events/log.rs::SessionLog::read_from). The replay half of subscribe.
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
