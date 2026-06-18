import { Agent, type Connection, type ConnectionContext, type WSMessage } from "agents";
import { getSandbox } from "@cloudflare/sandbox";
import type { Actor, Event, Payload } from "./contract.js";
import { bearerToken, verifyActorToken } from "./auth.js";
import { OpencodeMapper } from "./opencode_mapper.js";
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

// The actor for agent-produced events (the opencode /event stream the consume
// path maps into §0). Stamped by the GATEWAY, never self-reported by
// opencode-in-the-container — the §0 trust boundary (a compromised guest can't
// claim a different actor). Mirrors the local libkrun path's Actor::agent("opencode")
// (contract.rs prefixes the id with `a:`), so the same turn reads identically
// whether it ran on libkrun or managed CF.
const AGENT_ACTOR: Actor = { kind: "agent", id: "a:opencode" };

// opencode consume-path constants — kept aligned with src/sandbox/opencode.rs.
// The working directory the in-container opencode session operates in.
const OPENCODE_DIR = "/workspace";
// The port the in-container `opencode serve` binds (matches src/sandbox/opencode.rs).
const OPENCODE_PORT = 4096;
// Default model (`provider/modelID`) when neither /input nor OPENCODE_MODEL sets one.
const DEFAULT_MODEL = "zai-coding-plan/glm-4.5-air";
// Cap one driven turn's §0 appends so a runaway/looping agent can't grow the DO
// log without bound (the spike's blast-radius guard; generous for a single turn).
const MAX_TURN_EVENTS = 2000;
// Wall-clock cap on one driven turn — bounds how long the DO holds the long-lived
// /event fetch open if the agent never goes idle (Open Question #2's failure mode).
const TURN_TIMEOUT_MS = 300_000; // 5 min
// A workspace restore/backup over R2 (rustic) moves the whole tree through the
// container, far exceeding a turn's interactivity — give it a generous wall clock.
const WORKSPACE_XFER_TIMEOUT_MS = 300_000; // 5 min

// The rustic-on-R2 repo coordinates + resolved creds the host hands the container
// to restore the run's workspace in / snapshot results out (mirrors Rust S3Config).
// `access_key`/`secret_key` (and the separate `password`) are SECRET — they reach
// `pillbox workspace restore|backup` ONLY via the exec ENV, never argv, never §0.
type WorkspaceRepo = {
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  access_key: string;
  secret_key: string;
};

// Minimal shape of the opencode `config` overlay we construct/merge (the full
// type lives in @opencode-ai/sdk, not installed here). Provider → { options.apiKey }.
type OcConfig = {
  provider?: Record<string, { options?: { apiKey?: string } }>;
} & Record<string, unknown>;

// Payload types a producer may NOT submit via /event — each is a human/gateway
// action with its own authenticated route (arbitration / /input / /annotation /
// the grader), so accepting them on the open producer channel would let a token
// forge them.
const PRODUCER_FORBIDDEN = new Set(["driver_changed", "input", "annotation", "scored"]);

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

  // HTTP: POST /event, /input, /annotation and /driver/release land here
  // (routeAgentRequest forwards the request; WS upgrades go to onConnect instead).
  // Dispatch on the trailing path.
  async onRequest(req: Request): Promise<Response> {
    const path = new URL(req.url).pathname;
    if (path.endsWith("/event")) return this.handleEvent(req);
    if (path.endsWith("/input")) return this.handleInput(req);
    if (path.endsWith("/annotation")) return this.handleAnnotation(req);
    if (path.endsWith("/driver/release")) return this.handleRelease(req);
    if (path.endsWith("/provision")) return this.handleProvision(req);
    if (path.endsWith("/finalize")) return this.handleFinalize(req);
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

  // Driver arbitration (milestone 4): authZ on top of the actor authN. §0 allows
  // one driver at a time. Returns a 409 `Response` for the caller to return (same
  // convention as `requireActor`), or `null` when `actor` may drive — claiming a
  // free slot or stealing an occupied one (driver_changed granted|stolen); a no-op
  // when `actor` is already the driver.
  private async ensureDriver(actor: Actor, wantsSteal: boolean): Promise<Response | null> {
    const driver = await this.currentDriver();
    if (!driver) {
      await this.setDriver(actor);
      this.emitDriverChange(undefined, actor, "granted");
    } else if (!actorsEqual(driver, actor)) {
      if (!wantsSteal) return json({ error: "not the driver", driver }, 409);
      await this.setDriver(actor);
      this.emitDriverChange(driver, actor, "stolen");
    }
    return null;
  }

  // The gateway — not the driver — authors driver_changed events, so they're
  // stamped `system`. Single source of the transition's §0 shape.
  private emitDriverChange(
    from: Actor | undefined,
    to: Actor | undefined,
    mode: "granted" | "stolen" | "released",
  ): Event {
    return this.append(nowRfc3339(), SYSTEM_ACTOR, { type: "driver_changed", from, to, mode });
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
    // /event is the agent-output producer channel. Payload types that carry their
    // own authority — arbitration state (`driver_changed`), the driver-attributed
    // steer (`input`), the verifiable reward (`scored`) — have dedicated
    // authenticated paths (ensureDriver / /input / the grader). Reject them here so
    // a producer token can't forge them into the §0 log through the wrong door.
    if (PRODUCER_FORBIDDEN.has((body.payload as Payload).type)) {
      return json({ error: `payload type '${(body.payload as Payload).type}' not allowed on /event` }, 403);
    }
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
      model?: string;
    };

    // Arbitration gates the drive (who MAY drive, not just who they are). The
    // steal signal rides `?steal=1` or body `mode:"steal"` — a request-only flag,
    // not stored on the event (`Input` is always a discrete turn).
    const wantsSteal = body.mode === "steal" || new URL(req.url).searchParams.get("steal") === "1";
    const denied = await this.ensureDriver(actor, wantsSteal);
    if (denied) return denied;

    const target = (body.target as "agent" | "pty" | "exec") ?? "exec";
    const payload: Payload = { type: "input", text: body.text ?? "", target };
    const inEv = this.append(nowRfc3339(), actor, payload);
    // Drive the container only when one is bound (the container config). On the
    // free/§0-only deploy there's no Sandbox binding, so `/input` is append-only
    // — the attributed-input §0 path still works, just without the exec hop.
    if (this.env.Sandbox) {
      const model = body.model ?? this.env.OPENCODE_MODEL ?? DEFAULT_MODEL;
      await this.driveSandbox(this.env.Sandbox, inEv.seq, body.text ?? "", target, model);
    }
    return json({ seq: inEv.seq, head: this.head() });
  }

  // The async "chime in": an attributed comment that does NOT drive. Authenticated
  // (stamped with the verified actor, body-supplied actor ignored), but — unlike
  // /input — NOT driver-gated: any participant may annotate without holding the
  // driver slot. This is how the peanut gallery contributes; an orchestrator may
  // inject these as agent context.
  private async handleAnnotation(req: Request): Promise<Response> {
    const actor = await this.requireActor(req);
    if (actor instanceof Response) return actor;
    const body = (await req.json()) as { text?: string; anchor?: string };
    const payload: Payload = { type: "annotation", text: body.text ?? "", anchor: body.anchor };
    const ev = this.append(nowRfc3339(), actor, payload);
    return json({ seq: ev.seq, head: this.head() });
  }

  // The DO↔container hop. One container per session (addressed by the same
  // sessionId). Resolves the Sandbox-SDK handle, then dispatches on the input's
  // target: `agent` drives a real opencode turn and streams its /event SSE into §0
  // (the consume path); anything else runs the text as a one-shot command and
  // appends its output as a §0 tool_call (the exec round-trip). Takes the binding
  // explicitly (non-optional) so "only with a container bound" is in the type.
  private async driveSandbox(
    sandboxNs: NonNullable<Env["Sandbox"]>,
    inputSeq: number,
    text: string,
    target: "agent" | "pty" | "exec",
    model: string,
  ): Promise<void> {
    const sandbox = getSandbox(sandboxNs, this.name);
    if (target === "agent") {
      await this.driveAgent(sandbox, text, model);
    } else {
      await this.driveExec(sandbox, `exec-${inputSeq}`, text);
    }
  }

  // The exec round-trip: run the input as one command, append its output as a §0
  // tool_call. Cold-start: the container DO boots on first use and `exec` throws a
  // transient "Container is starting" until it's up — retry that case with a short
  // backoff; surface any non-transient error as-is.
  private async driveExec(
    sandbox: ReturnType<typeof getSandbox>,
    opId: string,
    cmd: string,
  ): Promise<void> {
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

  // ── managed workspace placement (rustic-on-R2) ────────────────────────
  // `pillbox run --backend managed` snapshots its cwd into a rustic repo on R2,
  // then POSTs /provision so the container restores that snapshot into /workspace
  // BEFORE the agent is driven; /finalize snapshots /workspace back to R2 after
  // the turn and returns the result handle. Both are driver actions (the host
  // holds the slot), so they're authenticated + driver-gated like /input.
  //
  // SECURITY: the R2 creds + repo password arrive in the request body and are
  // handed to the in-container `pillbox workspace …` ONLY via the exec ENV — never
  // on argv (visible in `ps`), never appended to the §0 log. The §0 events record
  // the non-secret coordinates (snapshot handle, target) + status.

  private async handleProvision(req: Request): Promise<Response> {
    const actor = await this.requireActor(req);
    if (actor instanceof Response) return actor;
    const denied = await this.ensureDriver(actor, false); // provisioning claims the slot
    if (denied) return denied;
    if (!this.env.Sandbox) {
      return json({ error: "no container bound — managed provisioning needs the Sandbox binding" }, 503);
    }
    const w = (
      (await req.json()) as {
        workspace?: { repo?: WorkspaceRepo; password?: string; snapshot?: string };
      }
    ).workspace;
    if (!w?.repo || !w.password || !w.snapshot) {
      return json({ error: "provision needs {workspace:{repo,password,snapshot}}" }, 400);
    }
    const sandbox = getSandbox(this.env.Sandbox, this.name);
    const res = await this.execWorkspaceTool(
      sandbox,
      workspaceCmd("restore", w.repo, w.snapshot),
      w.repo,
      w.password,
    );
    if (!res.ok) {
      this.appendWorkspaceXfer("workspace.restore", "failed", redactXfer(res.detail));
      return json({ error: `workspace restore failed: ${redactXfer(res.detail)}` }, 502);
    }
    this.appendWorkspaceXfer("workspace.restore", "completed", `restored ${w.snapshot} → ${OPENCODE_DIR}`);
    return json({ ok: true });
  }

  private async handleFinalize(req: Request): Promise<Response> {
    const actor = await this.requireActor(req);
    if (actor instanceof Response) return actor;
    const denied = await this.ensureDriver(actor, false);
    if (denied) return denied;
    if (!this.env.Sandbox) {
      return json({ error: "no container bound — managed finalize needs the Sandbox binding" }, 503);
    }
    const w = ((await req.json()) as { workspace?: { repo?: WorkspaceRepo; password?: string } }).workspace;
    if (!w?.repo || !w.password) {
      return json({ error: "finalize needs {workspace:{repo,password}}" }, 400);
    }
    const sandbox = getSandbox(this.env.Sandbox, this.name);
    const res = await this.execWorkspaceTool(sandbox, workspaceCmd("backup", w.repo), w.repo, w.password);
    if (!res.ok) {
      this.appendWorkspaceXfer("workspace.backup", "failed", redactXfer(res.detail));
      return json({ error: `workspace backup failed: ${redactXfer(res.detail)}` }, 502);
    }
    // `pillbox workspace backup` prints the new snapshot handle as its final stdout
    // line — that's the result handle the host records for `session pull`.
    const resultSnapshot = res.stdout.trim().split("\n").filter(Boolean).pop() ?? "";
    if (!resultSnapshot) {
      this.appendWorkspaceXfer("workspace.backup", "failed", "no snapshot handle on stdout");
      return json({ error: "workspace backup produced no snapshot handle" }, 502);
    }
    this.appendWorkspaceXfer("workspace.backup", "completed", `snapshot ${resultSnapshot}`);
    return json({ resultSnapshot });
  }

  // Run `pillbox workspace restore|backup` in the container with the R2 creds +
  // repo password injected ONLY via env (never argv/log). Retries the container
  // cold-start transient (mirrors driveExec).
  private async execWorkspaceTool(
    sandbox: ReturnType<typeof getSandbox>,
    cmd: string,
    repo: WorkspaceRepo,
    password: string,
  ): Promise<{ ok: boolean; detail: string; stdout: string }> {
    const env = {
      PILLBOX_R2_ACCESS_KEY: repo.access_key,
      PILLBOX_R2_SECRET_KEY: repo.secret_key,
      PILLBOX_REPO_PASSWORD: password,
    };
    let lastErr = "";
    for (let attempt = 0; attempt < 60; attempt++) {
      try {
        const res = await sandbox.exec(cmd, { env, timeout: WORKSPACE_XFER_TIMEOUT_MS });
        return {
          ok: res.success,
          detail: [res.stdout, res.stderr].filter(Boolean).join("\n"),
          stdout: res.stdout ?? "",
        };
      } catch (e) {
        lastErr = String(e);
        if (!/starting|not ready/i.test(lastErr)) break; // non-transient → stop
        await new Promise((r) => setTimeout(r, 1000)); // container cold-start backoff
      }
    }
    return { ok: false, detail: lastErr, stdout: "" };
  }

  // A §0 record of a workspace transfer — non-secret coordinates + status only.
  private appendWorkspaceXfer(name: string, status: string, output: string): void {
    this.append(nowRfc3339(), SYSTEM_ACTOR, {
      type: "tool_call",
      toolCallId: name,
      name,
      status,
      output,
    });
  }

  // The consume path (docs/managed-tier.md §Consume path): drive one real opencode
  // turn through the DO↔container hop and stream its /event SSE into §0 via the
  // OpencodeMapper. Boots opencode with the SDK's createOpencodeServer, opens the
  // event stream BEFORE prompting (opencode's /event is server-wide and emits from
  // connect time, so opening first can't miss the turn's opening events), creates a
  // fresh opencode session, drives it via prompt_async, then maps each SSE envelope
  // to §0 payloads — each stamped AGENT_ACTOR by the gateway, never self-reported
  // by the container. Stops when the turn goes idle (the mapper emits
  // attention_required) or at the per-turn event cap.
  //
  // Holds one long-lived SSE fetch for the whole turn — docs/managed-tier.md Open
  // Question #2 (DO↔container hop cost at streaming latency); the live falsifier
  // measures it. One opencode session per drive: cross-turn context (a persisted,
  // reused opencode session) is a follow-up beyond the one-turn falsifier.
  private async driveAgent(
    sandbox: ReturnType<typeof getSandbox>,
    text: string,
    model: string,
  ): Promise<void> {
    const [provider, modelId] = splitModel(model);
    if (!modelId) {
      this.appendAgentError(`model must be 'provider/modelID' (got '${model}')`);
      return;
    }
    const port = OPENCODE_PORT;
    // Own the boot. The SDK's createOpencodeServer mis-detects readiness inside the
    // CF container — its waitForPort(/path) times out though `opencode serve` binds
    // fine — so we launch the long-lived server with startProcess and poll /doc via
    // containerFetch (the production DO→container path) until it answers. This is the
    // Rust wait_ready pattern (src/sandbox/opencode.rs). opencode reads its provider
    // config from OPENCODE_CONFIG_CONTENT.
    let cfg: { config?: unknown; env: Record<string, string> };
    try {
      cfg = this.opencodeConfig();
    } catch (e) {
      this.appendAgentError(`opencode provider config: ${String(e)}`);
      return;
    }
    let probe = await this.probeDoc(sandbox, port);
    if (!probe.ok) {
      const env: Record<string, string> = { ...cfg.env };
      if (cfg.config !== undefined) env.OPENCODE_CONFIG_CONTENT = JSON.stringify(cfg.config);
      // Cold-start: a fresh container boots on first use and startProcess throws
      // "Container is starting" until it's up — retry that transient (mirrors
      // driveExec's cold-start handling; the agent path self-handles cold start so
      // it needs no separate warm-up).
      let startErr = "";
      let started = false;
      for (let attempt = 0; attempt < 60; attempt++) {
        try {
          await sandbox.startProcess(`cd ${OPENCODE_DIR} && opencode serve --port ${port} --hostname 0.0.0.0`, {
            env: Object.keys(env).length > 0 ? env : undefined,
          });
          started = true;
          break;
        } catch (e) {
          startErr = String(e);
          if (!/starting|not ready/i.test(startErr)) break; // non-transient → stop
          await new Promise((r) => setTimeout(r, 1000)); // container cold-start backoff
        }
      }
      if (!started) {
        this.appendAgentError(`opencode startProcess failed: ${startErr}`);
        return;
      }
      // Poll /doc until ready. Each probe is timeout-capped (probeDoc), so an
      // unreachable port fails fast instead of hanging the single-threaded DO.
      for (let i = 0; i < 20 && !probe.ok; i++) {
        await new Promise((r) => setTimeout(r, 1000));
        probe = await this.probeDoc(sandbox, port);
      }
      if (!probe.ok) {
        this.appendAgentError(`opencode not ready after boot (last probe: ${probe.detail})`);
        return;
      }
    }
    // Open the event stream first so no turn events are missed. Race the OPEN
    // against a timeout — a streaming fetch can't carry an AbortSignal without
    // capping the whole stream, so a stuck open fails loud instead of hanging.
    const evResp = (await Promise.race([
      this.ocFetch(sandbox, port, "GET", "/event", undefined, { accept: "text/event-stream" }),
      new Promise<null>((r) => setTimeout(() => r(null), 20000)),
    ])) as Response | null;
    if (!evResp) {
      this.appendAgentError("opencode /event open timed out (20s)");
      return;
    }
    if (!evResp.ok || !evResp.body) {
      this.appendAgentError(`opencode /event stream failed (HTTP ${evResp.status})`);
      return;
    }
    // Fresh opencode session, then drive it (bounded request/response calls).
    let ocSession: string;
    try {
      const created = await this.ocFetchT(sandbox, port, "POST", "/session", {}, 30000);
      if (!created.ok) {
        this.appendAgentError(`opencode create session failed (HTTP ${created.status})`);
        return;
      }
      const id = ((await created.json()) as { id?: string }).id;
      if (!id) {
        this.appendAgentError("opencode create session: no id in response");
        return;
      }
      ocSession = id;
    } catch (e) {
      this.appendAgentError(`opencode create session error: ${String(e).slice(0, 100)}`);
      return;
    }
    try {
      const prompted = await this.ocFetchT(
        sandbox,
        port,
        "POST",
        `/session/${ocSession}/prompt_async`,
        { parts: [{ type: "text", text }], model: { providerID: provider, modelID: modelId } },
        30000,
      );
      if (prompted.status < 200 || prompted.status >= 300) {
        this.appendAgentError(`opencode prompt failed (HTTP ${prompted.status})`);
        return;
      }
    } catch (e) {
      this.appendAgentError(`opencode prompt error: ${String(e).slice(0, 100)}`);
      return;
    }
    // Tail the SSE through the mapper → §0; stamped AGENT_ACTOR (the trust boundary).
    // Each read is raced against an idle timeout: `for await` would block on a silent
    // stream (the wall-clock check only runs after an envelope), so a non-streaming
    // proxy or a stalled turn would hang the DO. The idle case fails loud and
    // distinguishes "no data at all" (DO→container SSE not streaming) from a mid-turn
    // stall. TURN_TIMEOUT_MS still caps a long but live turn.
    const mapper = new OpencodeMapper();
    const deadline = Date.now() + TURN_TIMEOUT_MS;
    const IDLE_MS = 45000;
    let appended = 0;
    let sawData = false;
    const it = sseEnvelopes(evResp.body)[Symbol.asyncIterator]();
    for (;;) {
      const step = (await Promise.race([
        it.next(),
        new Promise<"idle">((r) => setTimeout(() => r("idle"), IDLE_MS)),
      ])) as IteratorResult<unknown> | "idle";
      if (step === "idle") {
        this.appendAgentError(
          sawData
            ? `agent turn stalled (no /event data for ${IDLE_MS / 1000}s)`
            : `no /event data in ${IDLE_MS / 1000}s — DO→container SSE not streaming`,
        );
        await it.return?.(undefined);
        break;
      }
      if (step.done) break;
      sawData = true;
      let done = false;
      for (const payload of mapper.onEvent(step.value)) {
        this.append(nowRfc3339(), AGENT_ACTOR, payload);
        if (payload.type === "attention_required") done = true; // turn went idle
        if (++appended >= MAX_TURN_EVENTS) done = true;
      }
      if (done) {
        await it.return?.(undefined);
        break;
      }
      if (Date.now() > deadline) {
        this.appendAgentError(`agent turn exceeded ${TURN_TIMEOUT_MS / 1000}s without going idle`);
        await it.return?.(undefined);
        break;
      }
    }
  }

  // One JSON request to the in-container opencode server via the Sandbox SDK's
  // DO→container primitive. containerFetch(request, port) proxies to
  // 127.0.0.1:port inside the container; the URL's host is ignored by the proxy,
  // so any absolute URL carrying the right path/method works.
  private ocFetch(
    sandbox: ReturnType<typeof getSandbox>,
    port: number,
    method: string,
    path: string,
    jsonBody?: unknown,
    headers?: Record<string, string>,
  ): Promise<Response> {
    const h: Record<string, string> = { ...headers };
    const init: RequestInit = { method };
    if (jsonBody !== undefined) {
      init.body = JSON.stringify(jsonBody);
      h["content-type"] = "application/json";
    }
    if (Object.keys(h).length > 0) init.headers = h;
    // No AbortSignal: containerFetch serializes the Request across the DO→container
    // hop and an AbortSignal isn't cloneable (DataCloneError). Callers that need a
    // deadline race a timer (ocFetchT / Promise.race) instead.
    return sandbox.containerFetch(new Request(`http://opencode${path}`, init), port);
  }

  // ocFetch bounded by a wall-clock race (containerFetch can't carry an AbortSignal).
  // Rejects on timeout so the caller's try/catch reports it; the losing containerFetch
  // leaks but resolves harmlessly.
  private ocFetchT(
    sandbox: ReturnType<typeof getSandbox>,
    port: number,
    method: string,
    path: string,
    jsonBody: unknown,
    timeoutMs: number,
  ): Promise<Response> {
    return Promise.race([
      this.ocFetch(sandbox, port, method, path, jsonBody),
      new Promise<never>((_, rej) => setTimeout(() => rej(new Error(`timeout ${timeoutMs}ms`)), timeoutMs)),
    ]);
  }

  // Probe opencode's /doc readiness endpoint via the containerFetch proxy, bounded by
  // a timer race so an unreachable port fails FAST instead of hanging (a hung probe
  // would wedge the single-threaded DO). Returns the outcome detail for a loud "not
  // ready" report (HTTP status vs the fetch error).
  private async probeDoc(
    sandbox: ReturnType<typeof getSandbox>,
    port: number,
  ): Promise<{ ok: boolean; detail: string }> {
    try {
      const resp = await this.ocFetchT(sandbox, port, "GET", "/doc", undefined, 5000);
      return { ok: resp.ok, detail: `HTTP ${resp.status}` };
    } catch (e) {
      return { ok: false, detail: `fetch ${String(e).slice(0, 100)}` };
    }
  }

  // opencode provider auth — passed to createOpencodeServer (managed-tier
  // Milestone 2: consume the managed secret store, not our MITM vault).
  // OPENCODE_CONFIG_JSON carries an explicit opencode `config` (a provider block
  // with an apiKey, or a CF AI Gateway); known provider keys are passed through as
  // env so opencode auto-detects them. Fails loud if neither is set — a
  // misconfigured run says so rather than letting opencode error opaquely mid-turn.
  private opencodeConfig(): { config?: unknown; env: Record<string, string> } {
    const env: Record<string, string> = {};
    // Providers opencode auto-detects from their standard env vars.
    for (const k of ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] as const) {
      const v = this.env[k];
      if (v) env[k] = v;
    }
    // The opencode `config.provider` overlay. An explicit OPENCODE_CONFIG_JSON wins;
    // ZAI_API_KEY is the ergonomic path for the GLM coding-plan subscription —
    // opencode knows `zai-coding-plan`'s base URL from models.dev, so only the apiKey
    // is needed (it's NOT a standard-env auto-detect provider, unlike anthropic), and
    // `??=` leaves an explicit config's own zai-coding-plan block untouched.
    let config: OcConfig | undefined = this.env.OPENCODE_CONFIG_JSON
      ? (JSON.parse(this.env.OPENCODE_CONFIG_JSON) as OcConfig)
      : undefined;
    if (this.env.ZAI_API_KEY) {
      config ??= {};
      config.provider ??= {};
      config.provider["zai-coding-plan"] ??= { options: { apiKey: this.env.ZAI_API_KEY } };
    }
    if (config === undefined && Object.keys(env).length === 0) {
      throw new Error(
        "no opencode provider configured — set ZAI_API_KEY (GLM coding plan), ANTHROPIC_API_KEY / OPENAI_API_KEY, or OPENCODE_CONFIG_JSON",
      );
    }
    return { config, env };
  }

  // A gateway-detected failure in the agent drive → a §0 attention_required so a
  // subscriber sees the turn ended (errored), not a silent hang. Stamped system —
  // the gateway detected it, not the agent.
  private appendAgentError(message: string): void {
    this.append(nowRfc3339(), SYSTEM_ACTOR, {
      type: "attention_required",
      reason: "error_stalled",
      message,
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
    const ev = this.emitDriverChange(actor, undefined, "released");
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

// Split `provider/modelID` on the first `/` (a model id may contain none → no model).
function splitModel(model: string): [string, string | undefined] {
  const i = model.indexOf("/");
  return i === -1 ? [model, undefined] : [model.slice(0, i), model.slice(i + 1)];
}

// Parse an opencode `/event` SSE body into JSON envelopes, in order — a port of
// src/events/opencode.rs::drain_sse. `data:` lines (one optional space after the
// colon; multiple in a frame joined with `\n`) accumulate until a blank line
// flushes the frame; a non-JSON frame is skipped (a stray frame can't wedge the
// stream). Workers' fetch de-chunks Transfer-Encoding, so there's no manual
// de-chunk (the Rust vsock path needs one). Cancels the reader on early return
// (break) so the long-lived containerFetch closes when the turn ends.
async function* sseEnvelopes(body: ReadableStream<Uint8Array>): AsyncGenerator<unknown> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  let data = "";
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (value) buf += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl).replace(/\r$/, ""); // tolerate CRLF
        buf = buf.slice(nl + 1);
        if (line.startsWith("data:")) {
          const rest = line.slice(5);
          if (data !== "") data += "\n";
          data += rest.startsWith(" ") ? rest.slice(1) : rest;
        } else if (line === "") {
          if (data !== "") {
            const frame = data;
            data = "";
            try {
              yield JSON.parse(frame);
            } catch {
              /* skip non-JSON frame, matching drain_sse */
            }
          }
        }
        // event: / id: / retry: / :comment lines carry no payload here.
      }
      if (done) break;
    }
    // A stream that closed mid-frame (no trailing blank line) still flushes.
    if (data !== "") {
      try {
        yield JSON.parse(data);
      } catch {
        /* skip */
      }
    }
  } finally {
    try {
      await reader.cancel();
    } catch {
      /* already closed */
    }
  }
}

function nowRfc3339(): string {
  return new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
}

// Build a `pillbox workspace restore|backup` command. Non-secret coordinates go
// on argv (single-quoted); the creds + password are passed via env by the caller
// (NEVER here), so this string is safe to keep in the §0 log. `backup` omits the
// snapshot (it creates one).
function workspaceCmd(mode: "restore" | "backup", repo: WorkspaceRepo, snapshot?: string): string {
  const args = [
    "pillbox",
    "workspace",
    mode,
    "--endpoint",
    sq(repo.endpoint),
    "--bucket",
    sq(repo.bucket),
    "--region",
    sq(repo.region),
    "--prefix",
    sq(repo.prefix),
  ];
  if (snapshot) args.push("--snapshot", sq(snapshot));
  args.push("--target", sq(OPENCODE_DIR));
  return args.join(" ");
}

// Single-quote a coordinate for the exec shell string. The values are from the
// user's own trusted pillbox config, but quote defensively all the same.
function sq(v: string): string {
  return `'${v.replace(/'/g, "'\\''")}'`;
}

// The creds never reach the command's output (they're env-only), but cap the
// transfer-error detail logged to §0 so a verbose rustic dump can't drown replay.
function redactXfer(detail: string): string {
  return detail.length > 2000 ? `${detail.slice(0, 2000)}…` : detail;
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body) + "\n", {
    status,
    headers: { "content-type": "application/json" },
  });
}
