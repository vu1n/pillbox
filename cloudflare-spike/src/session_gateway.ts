import { Agent, type Connection, type ConnectionContext, type WSMessage } from "agents";
import { getSandbox } from "@cloudflare/sandbox";
import type { Actor, Event, Payload } from "./contract.js";
import { bearerToken, verifyActorToken } from "./auth.js";
import type { Env } from "./worker.js";

// Per-connection state (the subscriber's replay cursor + its authenticated
// actor), persisted across hibernation via `connection.setState` — the Agents-SDK
// replacement for raw `serializeAttachment`. NOTE: this is per-connection state,
// NOT the global `this.setState`. We deliberately NEVER call global
// `this.setState`: it is last-write-wins whole-object sync, structurally
// incompatible with an ordered append-only log keyed by monotonic `seq` (it'd
// lose ordering, replay, and per-event actor attribution). The §0 log lives in
// `this.ctx.storage.sql`; the cursor lives here. (This is the exact split GSV
// runs in production.) `actor` is the connection's verified identity (`undefined`
// for an anonymous read-only subscriber).
type WsState = { role: "subscriber"; cursor: number; actor?: Actor };

// pillbox itself — the actor for events the gateway originates (the container-hop
// exec result), as opposed to a producer- or human-submitted event.
const SYSTEM_ACTOR: Actor = { kind: "system", id: "pillbox" };

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
  private append(at: string, actor: Actor | undefined, payload: Payload): Event {
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
    if (actor) ev.actor = actor;
    this.fanout(ev);
    return ev;
  }

  // HTTP: POST /event, /input and /driver/release land here (routeAgentRequest
  // forwards the request; WS upgrades go to onConnect instead). Dispatch on the
  // trailing path.
  async onRequest(req: Request): Promise<Response> {
    const path = new URL(req.url).pathname;
    if (path.endsWith("/event")) return this.handleEvent(req);
    if (path.endsWith("/input")) return this.handleInput(req);
    if (path.endsWith("/driver/release")) return this.handleRelease(req);
    return new Response("not found\n", { status: 404 });
  }

  // ── driver arbitration (authZ over authN) ─────────────────────────────
  // auth.ts answers "who are you" (authN — the verified actor). This answers "are
  // you ALLOWED to drive" (authZ): §0 permits one driver at a time, so the
  // attributed-input path must gate on whether the *verified* actor currently
  // holds the single driver slot. The decision keys off the trusted, token-stamped
  // actor — never a body-supplied identity.
  //
  // The driver is DURABLE state, not a per-connection or in-memory field: it must
  // outlive DO hibernation/eviction so a reattaching steerer (HTTP /input has no
  // long-lived socket) sees a consistent "who is driving" across calls. Stored as
  // a single durable KV key alongside the §0 log.
  private static readonly DRIVER_KEY = "driver";

  private async currentDriver(): Promise<Actor | undefined> {
    return (await this.ctx.storage.get<Actor>(SessionGateway.DRIVER_KEY)) ?? undefined;
  }

  private async setDriver(actor: Actor | undefined): Promise<void> {
    if (actor) await this.ctx.storage.put(SessionGateway.DRIVER_KEY, actor);
    else await this.ctx.storage.delete(SessionGateway.DRIVER_KEY);
  }

  // The /event producer path → EventLog::append. A producer (the in-sandbox §0
  // tailer, a host process) submits an Event; this is the resident-sequencer
  // analogue of a co-located producer calling SessionLog::append. Note the
  // producer-supplied `body.seq` is DISCARDED — `append` stamps the authority's
  // seq, matching SessionLog::append's overwrite-the-producer's-seq rule.
  //
  // The trust boundary (session-event-log.md §Actor model). The actor is derived
  // from a verified `Authorization: Bearer <token>` credential, NEVER from the
  // request body — a body-supplied `actor` is ignored as spoofed. Writes require a
  // valid token (401 otherwise), so only a holder of the issuer's secret can
  // append attributed events; authz (who may drive/approve/join) keys off the
  // stamped, trusted actor.
  private async handleEvent(req: Request): Promise<Response> {
    const actor = await this.requireActor(req);
    if (actor instanceof Response) return actor;
    const body = (await req.json()) as Partial<Event>;
    if (!body.payload) return json({ error: "missing payload" }, 400);
    const ev = this.append(body.at ?? nowRfc3339(), actor, body.payload as Payload);
    return json({ seq: ev.seq, head: this.head() });
  }

  // The write-path auth gate (single source of the "writes require a valid token"
  // policy): the verified actor, or a 401 Response for the caller to return.
  private async requireActor(req: Request): Promise<Actor | Response> {
    const actor = await this.verifiedActor(bearerToken(req));
    return actor ?? json({ error: "unauthenticated" }, 401);
  }

  // Verify a token against the issuer secret, returning the attested actor (or
  // `null` = unauthenticated). Fails CLOSED: with no `ACTOR_TOKEN_SECRET`
  // configured there's nothing to verify against, so no actor can be attested.
  private async verifiedActor(token: string | null): Promise<Actor | null> {
    const secret = this.env.ACTOR_TOKEN_SECRET;
    if (!secret || !token) return null;
    return verifyActorToken(token, secret);
  }

  // Attributed input — the durable steer, then driven into the container. The
  // input is appended (seq N, fans out), the call crosses the DO↔container hop
  // (the one unmeasured managed-tier risk), and the result is appended as a §0
  // event (seq N+1) so a subscriber sees the round-trip: input → output.
  private async handleInput(req: Request): Promise<Response> {
    // Driving is attributed + authenticated: the steer is stamped with the
    // verified actor (the body's `actor` is ignored), 401 without a valid token.
    const actor = await this.requireActor(req);
    if (actor instanceof Response) return actor;
    const body = (await req.json()) as {
      text?: string;
      target?: string;
      mode?: string;
    };

    // Driver arbitration (milestone 4): authZ layered on the authN above. The
    // verified `actor` is established; here we decide whether that actor may
    // actually drive. §0 allows one driver at a time:
    //   - no current driver        → this actor CLAIMS it (driver_changed granted)
    //   - current driver IS actor   → proceed, no event
    //   - current driver is OTHER   → 409 unless the request opts to steal
    //                                 (body `mode:"steal"` or `?steal=1`), in which
    //                                 case reassign (driver_changed stolen)
    const wantsSteal = body.mode === "steal" || new URL(req.url).searchParams.get("steal") === "1";
    const driver = await this.currentDriver();
    if (!driver) {
      await this.setDriver(actor);
      this.append(nowRfc3339(), SYSTEM_ACTOR, { type: "driver_changed", to: actor, mode: "granted" });
    } else if (!actorsEqual(driver, actor)) {
      if (!wantsSteal) {
        return json({ error: "not the driver", driver }, 409);
      }
      await this.setDriver(actor);
      this.append(nowRfc3339(), SYSTEM_ACTOR, {
        type: "driver_changed",
        from: driver,
        to: actor,
        mode: "stolen",
      });
    }
    // else: current driver is this actor — proceed, no driver_changed event.

    const payload: Payload = {
      type: "input",
      text: body.text ?? "",
      target: (body.target as "agent" | "pty" | "exec") ?? "exec",
      // `body.mode` is overloaded: it can also carry the "steal" arbitration
      // signal, which is NOT a valid input mode. Only "live" maps through;
      // everything else (incl. "steal", undefined) is the default "turn".
      mode: body.mode === "live" ? "live" : "turn",
    };
    const inEv = this.append(nowRfc3339(), actor, payload);
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

  // Append the container exec's outcome as a §0 tool_call event (fans out to
  // subscribers). The gateway originated this (the container hop), so it's stamped
  // `system`, not the driver's actor.
  private appendExec(opId: string, status: string, output: string): void {
    this.append(nowRfc3339(), SYSTEM_ACTOR, {
      type: "tool_call",
      toolCallId: opId,
      name: "exec",
      status,
      output,
    });
  }

  // Release the driver slot — the voluntary give-up half of arbitration. AuthZ:
  // only the *current* driver may release (a non-driver gets 409, an
  // unauthenticated caller 401). Clears the durable driver and appends
  // `driver_changed {from: actor, mode: "released"}` (no `to`: the slot is now
  // free, so the next /input claims it via "granted").
  private async handleRelease(req: Request): Promise<Response> {
    const actor = await this.requireActor(req);
    if (actor instanceof Response) return actor;
    const driver = await this.currentDriver();
    if (!driver || !actorsEqual(driver, actor)) {
      return json({ error: "not the driver", driver: driver ?? null }, 409);
    }
    await this.setDriver(undefined);
    const ev = this.append(nowRfc3339(), SYSTEM_ACTOR, {
      type: "driver_changed",
      from: actor,
      mode: "released",
    });
    return json({ seq: ev.seq, head: this.head() });
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
  // The WS half of the same trust boundary as handleEvent. The connection's actor
  // is derived from a verified `?token=` on the upgrade URL (browsers can't set
  // headers on a WS handshake, so the credential rides the query) and bound to the
  // connection in WsState — so a future socket-driven /input is stamped with the
  // connection's authenticated actor, never a body-supplied one. Reads stay open:
  // an anonymous subscriber (no/invalid token) may watch, just with `actor`
  // undefined; it's the WRITE paths that require attestation.
  async onConnect(connection: Connection<WsState>, ctx: ConnectionContext): Promise<void> {
    const url = new URL(ctx.request.url);
    const actor = (await this.verifiedActor(url.searchParams.get("token"))) ?? undefined;
    const from = Number(url.searchParams.get("from") ?? "0");
    let cursor = from - 1;
    for (const ev of this.readFrom(from)) {
      connection.send(JSON.stringify(ev));
      cursor = ev.seq;
    }
    connection.setState({ role: "subscriber", cursor, actor });
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
        // Preserve the connection's authenticated actor across a re-replay.
        connection.setState({ ...(connection.state ?? { role: "subscriber" }), cursor });
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

// Actor identity for arbitration: same principal iff (kind, id) match. `display`
// is cosmetic and ignored — two tokens for the same id are the same driver.
function actorsEqual(a: Actor, b: Actor): boolean {
  return a.kind === b.kind && a.id === b.id;
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
