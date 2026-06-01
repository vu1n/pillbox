# pillbox vNext — the multiplayer substrate

> **⚠️ DIRECTION UPDATE (2026-06-01) — read this first.** Two strategic moves
> postdate most of this doc; where the body conflicts, these win:
> 1. **Local is the sauce; remote is extra.** pillbox is a *local-first* tool —
>    a secure, fast-loading sandbox you run on your machine and drive from chat,
>    with great telemetry. "Remote" is now *Cloudflare-managed* (their Sandbox
>    SDK / Claude Managed Agents own the managed substrate — see memory
>    `pillbox-cloudflare-shipped-the-stack`) or *pillbox-running-locally-on-the-
>    VPS*. The SSH/e2b/`docker://` remote backends are **deprecated**.
> 2. **Substrate: Docker → libkrun microVM.** The local runtime pivots to
>    [libkrun-sandbox.md](./libkrun-sandbox.md) (secure VM boundary, fast, no
>    daemon, macOS-native). [remotes-redesign.md](./archive/remotes-redesign.md) (the
>    Docker-context collapse) is **superseded** by it.
>
> What stands from the body below: the **§0 event spine + gateway** (the
> keystone), the **drive surface** (`session send/watch/subscribe`), and
> agent-agnostic multi-agent support — all transport-agnostic, so they port onto
> libkrun unchanged. The differentiators are **drive-from-chat + telemetry**, not
> the sandbox itself.

Status: umbrella / roadmap. Owns the strategic frame, the layering model,
and the **one** unified sequence. Indexes the deep specs; does not
duplicate them.

Deep specs:
- [libkrun-sandbox.md](./libkrun-sandbox.md) — **the substrate** (libkrun
  microVM; supersedes the Docker/remote line).
- [session-event-log.md](./session-event-log.md) — the durable, attributed
  session spine (keystone).
- [gateway.md](./gateway.md) — the per-session sequencer + broker + attach
  endpoint that §0 actually gates on.
- ~~remotes-redesign.md~~ — *superseded, archived*; the Docker-context backend
  collapse is retired (see the banner above + [libkrun-sandbox.md](./libkrun-sandbox.md)).
- [dx.md](./dx.md) — the developer-experience contract (the bundle is the
  product): the three inner loops + the zero-config-local principle.
- *orchestrator / optimization layer* — a **separate project** that consumes
  the pillbox contract (not in this repo); the run-time half is specced in
  [swarm-memory.md](./swarm-memory.md). See
  *Optimization & collective intelligence* below.

## Why vNext exists

pillbox already has Aquifer's architecture (session / harness / sandbox,
credentials proxy; **profiles are *specified* but not yet built — 0 product
surface today, see [dx.md](./dx.md)**) — independently validated by Shopify's
"Under the River" (May 2026). The pressure-test is equally clear: there
is **no single-feature moat** — vault, sandboxing, observability, and
detach are each commoditized by funded/free competitors. The value is the
*integrated bundle*, and the one thing the bundle is missing is the thing
Aquifer's whole thesis turns on: **circulation** — public, multiplayer,
compounding use.

> vNext's job: turn pillbox from a single-player local runner into a
> **multiplayer substrate** with portable placement and a path to a
> managed service.

Not a feature race. The architecture is done; vNext adds circulation,
portability, and a monetization path on top of it — and, via the
optimization layer, the first piece that *compounds*: a self-improving
harness whose value grows with use. That's the one thing a competitor
giving away static features cannot hand out.

## Build / defer / cut (post-review, 2026-05-30)

An adversarial review (every code-claim verified against the source + OSS
prior-art) kept the direction but reordered confidence. The *sequencing and
scope* change:

- **~~Remotes collapse — BUILD FIRST~~ → RETIRED.** This said "lean on Docker
  contexts; collapse ssh/e2b onto `docker://`." Both halves are dead: the local
  runtime pivots to **libkrun** ([libkrun-sandbox.md](./libkrun-sandbox.md)), and
  "remote" is now Cloudflare-managed / pillbox-local-on-the-box, not an
  SSH-driven daemon. **BUILD FIRST is now the libkrun substrate** (proof-first
  boot → `pillbox-init` → port §0/attach onto vsock).
- **Harden #1 (transcript+MITM source of truth) + #3 (native secondary) —
  BUILD.** Cheap, real.
- **Harden #2 (`raw_body` blob store) — DEFER.** Net-new infra that *reverses*
  a deliberate memory-discard (`genai_tap.rs:193`); its only consumer is the
  externalized optimization layer.
- **Multiplayer read-only fan-out + web-attach — BUILD, but demoted.** The
  multi-human circulation demo; rides the existing frame protocol — but it
  sequences *after* the cheaper local-visibility work, since the §0 local
  subscribe surface + a thin reader already give single-player "watch your
  agent."
- **Multiplayer input / roles / topologies — DEFER** until fan-out shows pull
  (users are overwhelmingly solo; multi-writer keystroke arbitration solves a
  problem almost nobody has yet).
- **Managed tier — DEFER.** One thin adapter behind the existing trait, gated on
  demonstrated demand; the value is the bundle, the compute is commodity resale.
- **Optimization & collective intelligence — CUT from this repo** (honor the
  doc's own "separate project" scoping). Each piece ships externally
  (meta-harness is OSS; routing is commoditized; DSPy-for-coding is unproven).
  pillbox's role is the substrate the loop runs *on* — which it already is.

Minimal path that proves the thesis: the **libkrun substrate** + the §0
**local subscribe surface** + a thin `pillbox watch` reader (the cheapest "watch
your agent") + drive-from-chat. The multi-human web-attach demo follows;
everything else earns its place after that.

## The layering (what makes the specs cohere)

Four layers, longest-lived first. This reconciles the event-log spec
("session is the durable identity") with the remotes spec ("the container
is the primitive") — both true, at different layers.

| Layer | What | Lifetime | Spec |
|---|---|---|---|
| **Session** | durable identity; the event-log spine; partition key | outlives everything | event-log |
| **Run / sandbox** | one agent execution; lineage via `parent_run_id`; `base → result` snapshots | per task | event-log + rustic |
| **Container / placement** | *where* it runs: local **libkrun microVM** (default) / managed (Cloudflare). ~~`docker://`/`k8s://`~~ retired | disposable | [libkrun-sandbox](./libkrun-sandbox.md) |
| **Gateway** | the convergence point — **sequencer** (assigns `seq`) + **participant broker** (auth / roster / input arbitration) + **placement attach** | per session | multiplayer |

**Invariant:** session > run > container in lifetime. Detach/reattach,
replay, and result-pull key off `sessionId`, never the container — a
replaced container must not look like a lost session. (This is the
session-vs-container reconciliation the remotes doc flags as an open
question; it is *resolved here* and the specs inherit it.)

**§0 is the local substrate — done. The structural re-model is a
multiplayer/migration prerequisite, re-scoped OUT of §0 (2026-05-31).** §0 as
*built* is: `sessionId` on the Event + the durable per-session log + the
co-located single-writer sequencer + the zero-config local `Subscribe(from_seq)`
surface + a producer. That's the part every consumer reads, and it's complete +
live-verified. Three harder pieces were *originally* lumped into §0; they have
**no consumer in the single-player product** and are reclassified to land with
their drivers:
- **(b) merge the two event systems** (rich `contract.rs` vs lifecycle
  `events/mod.rs`) + **`actor` on the envelope** + **gateway as network
  seq-authority** → **Multiplayer** (the attributed multi-writer thread is what
  needs them). The fan-out review confirmed: until then, the per-session log is
  the bus and consumers are read-side exporters; folding lifecycle onto the seq
  spine would force the deferred host↔sandbox seq handoff for no current gain.
- **(c) re-model `Session` from 1:1-with-a-sandbox into a cross-sandbox spine**
  → **Remotes / managed migration** ("session migrates local → docker:// →
  managed"). Detach/reattach + the per-session log already key off the durable
  `Session.id`, so the 1:1 record is fine until a session genuinely spans
  sandboxes.
- **`class: content|signal`** → the pooling/optimization track.

Net: don't build the structural §0 preemptively; it ships with multiplayer
(b/actor/seq) or migration (c). §0-the-substrate is closed.

**The gateway is one *role* — but the hardest net-new build, not "build it
once and done."** Sequencer + broker + attach can be one process, but none of
it exists: no per-session sequencer (seq is a per-*emitter* counter that resets
to 1 per run/exec — the "monotonic per pillbox" code comments were wrong, now
corrected), no actor-auth, and a stateful coordinator that outlives containers
sounds like a *resident broker* — in tension with pillbox's no-daemon identity.
**Now specified** in the gateway spec ([gateway.md](./gateway.md)): the
submit→seq wire contract, the connection-auth backing `actor`, the in-sandbox
producer token, the **no-daemon reconciliation** (the log on disk is the durable
spine; the gateway is an *ephemeral single-writer* that holds the append lock
only while a session is live), and the host↔sandbox seq-authority handoff.

## Workstreams

1. **Harden** — observability content capture. Small; see below.
2. **Session event log** — the keystone spine. → `session-event-log.md`
3. **Multiplayer** — actor, attributed input, participant/role/driver, the
   gateway, topologies. Roadmap below.
4. **Remotes** — container-is-primitive; BYO free / managed paid.
   → `remotes-redesign.md`
5. **Optimization & collective intelligence** — DSPy + meta-harness,
   cost-routing, the data flywheel. A separate project that consumes the
   contract. Section below.

### Harden (the three decisions)

1. Keep **transcript + MITM** as content source of truth — don't switch to
   native (it redacts reasoning, truncates at 60KB, and Codex emits nothing
   usable).
2. Persist the **full unredacted MITM bodies (incl. thinking)** as `raw_body`
   events → blob store, referenced by `bodyRef`. **Net-new, not "cheap":**
   today nothing persists bodies — `genai_tap.rs:193` deliberately *drops* the
   buffer on the first SSE event to avoid holding long-context bodies in memory,
   and conversation content reaches spans only via the transcript synth. So this
   reverses a memory-safety decision and adds a new at-rest store of unredacted
   reasoning (a new threat surface — add it to `security.md` first). Gated on §0
   (spine) + a content store: **reuse the existing rustic content-addressed
   store**, don't build a second. Its only consumer is the externalized
   optimization layer, so it can wait (DEFER).
3. Ingest **native metrics as secondary enrichment** (exact cost/LoC,
   stability fallback), gated and off the spine.

Decisions #1 and #3 are just event types on the log + an exporter. #2 is the
exception — a new MITM retain path; see above.

**Harden #4 — zero-config local *subscribe surface* (the biggest DX win, see
[dx.md](./dx.md)).** Not a UI — the substrate exposes streams, consumers
(lum / Slack / IDE) render. The transcript stream is *already* parsed into
harness-agnostic events, but the only consumer is an OTLP sink that no-ops
without a collector — so the structured stream a consumer would subscribe to
isn't locally available, and a plain `pillbox run` is blind by default. Expose
it **locally + zero-config** (`Subscribe(from_seq)` over a local socket/WS — the
§0 surface) + persist as JSONL; ship a *thin optional reference reader*
(`pillbox watch`, the `docker logs` model) over that public tap. A *surface*,
not a UI — and mostly a property of §0 done right.

### Multiplayer (roadmap)

- **Output fan-out** — N concurrent read-only attachers; late-join replay
  from latest `pty_snapshot` + tail. **Web attach (WS)** join-link → the
  first visible circulation demo.
- **Input** — driver-token arbiter for live keystrokes; turn-queue for
  programmatic; async **annotation** channel that doesn't take the keyboard;
  HTTP/gRPC endpoint to POST input (agents, webhooks, CI).
- **Identity / roles** — gateway-authenticated `actor`; owner/driver/
  observer/commenter; join links with scope + TTL; tool-approval routing.
- **Detached approval loop** (high-value, lands early — transport already
  exists via the coalesced Input-frame channel, commit d590a2d): a
  `session.blocked` lifecycle event the instant a gate is hit, shipped through
  the existing sinks, + `session approve|deny|answer` verbs + a Slack/webhook
  callback. Reversible gates auto-resume on TTL; irreversible (push/merge) hard-
  block. Closes the "agent blocked while you're away" gap (Claude Code #29438).
  See [dx.md](./dx.md).
- **Topologies** — N humans→1 agent (collab); 1 human→N agents (fleet
  broadcast / lum's canvas); N agents↔N agents (sub-agents as participants).

**Adopt, don't re-derive (sshx).** `ekzhang/sshx` already ships ~80% of this in
open-source Rust. Copy its protocol shape — per-stream `u64` seq, a
`SequenceNumbers` catch-up map, `SerializedShell` reconnect, and a **bounded
fan-out buffer** — *before* the web-attach demo. Two concrete prerequisites the
code lacks: (1) the `Frame` header is `type+len` only — pull `seq`/`ack` + a
`Hello` version field into it early (before the web-attach demo), not "phase
5"; (2) `host.rs` broadcast
is an unbounded `Vec<Sender>` and the specced `DataAck` window is **unbuilt** —
implement the bounded per-client window *before* N attachers, or the first
circulation demo is also the first OOM when a slow web client stalls. Emit the
**durable** terminal projection as **asciinema cast v2** (free player/embed);
keep the binary `Frame` protocol for the live hot path only. Lock
**driver-token** (host-authoritative, à la tmux / Live Share) for input — not a
CRDT. Do *not* copy sshx's E2E encryption: it forecloses the server-side screen
+ observability that are pillbox's reason to exist.

## Optimization & collective intelligence

> **Post-review status: CUT from this repo; pursue externally if at all.** Each
> piece already ships: `stanford-iris-lab/meta-harness` is open-source —
> **wrap it**, don't rebuild; cost-routing is commoditized (RouteLLM / LiteLLM
> / OpenRouter / native model routers) — **consume LiteLLM**, don't build a
> router; **GEPA needs a coarse *verifiable score*, not a labeled scalar** —
> rich textual feedback (stderr / failing tests / diffs / traces from the event
> log) carries the gradient, so MIPROv2's labeled-scalar regime is what
> open-ended coding lacks and GEPA sidesteps. The catch: the score must be
> **externally graded, never the self-reported `session.completed`**, and use the
> standalone `gepa` library / `optimize_anything` (not `dspy.GEPA`, which only
> tunes DSPy predictor fields — your scaffold is non-DSPy text). The real moat
> is **privacy** of cross-user pooling (cf. FedPOB), not the optimizer — naive
> pooling leaks exactly the code/prompts pillbox exists to isolate. **pillbox's
> role is the trace-rich, reproducible, secret-isolated substrate the loop runs
> *on* — which it already is.** Ship per-pillbox playbooks first; gate any
> cross-user pooling behind a privacy design + an independent verifier. Treat
> "bacchus's jj engine is the hard 80%" as an *assumption to validate against
> bacchus's actual scope*, not a given.
>
> **Factoring (cf. aithy / Ax / RLM-style flow):** push the deterministic work
> — parsing, filtering, retrieval, dedup, tool-routing — into *code*; let the
> model do only language + judgment. That's *why* this layer's optimization
> surface stays small and tractable (DSPy/GEPA tune only the judgment prompts,
> not the scaffold), and it's the same "deterministic routing before the agent"
> the gateway broker already does. The substrate exposes the deterministic
> primitives (retrieval, snapshots, tool routing); the optimizer tunes the thin
> judgment layer on top.

The layer that turns the bundle into something that *compounds* — the first
piece of vNext with a durable moat rather than parity. It is a **separate
project that consumes the pillbox contract**, never a fork.

### Two gateways — don't muddle them

- **Session gateway** (the layering above): per-session, *inside* pillbox —
  sequencer + broker + attach. Owns one session.
- **Swarm orchestrator** (this layer): *above* many pillboxes — decompose a
  goal, route to N sessions (different placements/models), fan results in. A
  pillbox *client* speaking the proto contract (`Spawn`/`Subscribe`/
  `SendInput`). Separate repo.

The proto already named this consumer ("orchestrators … own their own
threads; they only speak this contract"). Keep the membrane clean and both
stay simple.

### Two optimization layers (complementary, not competing)

- **DSPy** — optimizes the **prompt / brief transform** (input → agent
  brief): instruction + demo tuning.
- **Meta-harness-style proposer** — optimizes the **scaffold / profile**
  (tools, memory, context policy, model): structural search. A pillbox
  **profile *is* a harness**, so this is "profiles that tune themselves."

Both read the event log; both emit shareable (scrubbed) artifacts.

**Reuse, don't rebuild:** the hard 80% of the orchestrator is
**fan-in/merge** on a shared codebase — that's bacchus's jj coordination
engine. Consume it; keep the orchestrator thin (router + bandit +
aggregator).

### Phasing

- **Step 1** — DSPy gateway up: a hand-authored program as the transform
  layer. No optimization, no collection. Just structure.
- **Step 1.5** — optimize **offline** on local logs / a benchmark: DSPy on
  the prompt, meta-harness on profiles. Still local, still sovereign.
- **Step 2** — **cost-routing as a contextual bandit**: A/B models to
  *explore*, route on the learned `task-features → cheapest-model-that-
  succeeds` policy to *exploit*. Opt-in token spend. Cleanest-metric wedge
  (cost per success + implicit intervention/retry), and it fixes the
  managed-tier value story: "cheapest model that succeeds — X% lower spend
  at equal success," a dollar number, not a cheaper sandbox.

### Data principles (load-bearing — this is the trust boundary)

The flywheel needs data; collecting it wrong torches the only moat (trust).

1. **Local capture, always.** The event log on the user's machine improves
   *their* harness. Trust-safe, already the design.
2. **No central collection by default.** Pooling is **opt-in**, content-
   scrubbed, and naturally the **managed-tier boundary**: free/BYO/local =
   sovereign, nothing leaves; managed = opt-in pooling as an explicit value
   exchange. Matches the codebase's conservative stance ("collector
   reachability is the user's job — no host relay").
3. **Pool signal, not content.** Schema splits `content` (code/prompts →
   local only) from `signal` (task features + outcomes → poolable). Cost-
   routing needs the metadata, not your code.
4. **Share artifacts, not data (the federated line).** Share **tuned
   instructions + policy params + metric stats**; **never bootstrapped
   few-shot demos** — they embed user content. Exclude or synthesize them;
   scrub instructions for embedded specifics (reuse the vault redaction
   muscle). Same rule for meta-optimized profiles.
5. **Governance: open consumption / opt-in contribution / quality-gated
   inclusion.** Anyone can consume (baseline free; premium routing = the
   managed-tier carrot). Contribution is opt-in — **never required to
   consume** (that contradicts sovereignty and throttles the circulation
   we're short on). An artifact joins the collective only if it **improves
   the metric on a held-out eval** (anti-poisoning). Gate *inclusion* by
   quality, not *consumption* by contribution.

The difference from the extractive-mirage failure is **consent + value
returned** — regenerative circulation, not silent take.

### Data has three roles — keep them distinct

| Role | Source | Verifiable metric? |
|---|---|---|
| **Eval / metric** — does optimization help? | **Harbor** (the task format + harness built by the Terminal-Bench team) as the *interface*; **Terminal-Bench 2.0, SWE-bench, DeepSWE all run *through* Harbor** — they are datasets, not alternatives. | yes — the point |
| **Plumbing fixtures** — does ingestion work? | **HF agent-traces** (unlabeled, diverse shapes) | no — and that's fine |
| **Optimization volume** — the fuel | own **event log** + opt-in **pooled** collective | yes (your outcomes) |

Failure mode: unlabeled fixtures masquerading as signal — and a category error
earlier drafts made: Harbor does **not** "replace" Terminal-Bench; it is the
harness TB2/SWE-bench/DeepSWE all run through. Adopt **Harbor as the eval
interface**; pick datasets by contamination-resistance — **SWE-rebench**
(date-gated) + DeepSWE for optimization, **SWE-bench Verified as
never-optimize-against sanity only** (it is contaminated — fatal for an
optimization loop), Aider polyglot as the cheap inner-loop smoke metric. The
reward MUST be independently verifiable (tests-pass / CI-green / diff-applies)
— **never the self-reported `session.completed`** (Goodhart). The trainset
adapter must be format-pluggable from day 1 (Claude Code, Codex, *and* foreign
HF shapes). (Adopt upstream Harbor, not the low-traffic Pier fork.)

### Workshop

Not the gateway consumer. The "consumer" is a thin **trainset adapter over
the canonical event log**, not a daemon — its storage/ingest would
duplicate the log. Keep the OTLP export so Workshop (or Langfuse/Phoenix)
can attach as an **optional dev-time viewer**. One data path: the orchestrator
consumes pillbox events **over the contract** — `Subscribe` with its `from_seq`
replay cursor (`0` = live tail; verified in `agent.proto` — there is **no**
separate historical-read RPC), never a parallel ingest.

## The one unified sequence

Across all four workstreams, ordered by dependency. The point of a single
list: the specs each have their own internal order, but this is the order
that respects the cross-spec dependencies.

Re-sequenced after three reviews (architecture, DX, substrate-not-UI) onto one
principle: **do the cheap, mostly-independent things that make pillbox usable
and visible first; then the spine that generalizes them; then the heavy
multi-human surface.**

| # | Step | Why here / deps | Workstream |
|---|---|---|---|
| 1 | **Remotes collapse — `docker://` + cold-host DX contract** *(foreground run + ingest + result-extraction + sandbox-side vault ✓ live-verified; **`--detach` lifecycle + agent-stays-alive ✓ live-verified** — record/list/reattach/teardown-over-endpoint, detached agent runs + is reattachable; **version-skew detection ✓** — preflight probe fails loud if the runner image's pillbox is too old for the launch protocol, the real cause of an earlier "agent instantly dies"; **drive + read parity ✓** — `session send` drives a detached docker:// session's pty-host over the endpoint, and `session subscribe`/`watch` tail the container transcript out over `docker exec` into the host log (collector-free, the §0 surface); remaining: detached result-extraction, pull-progress)* | Cheapest big win; a *deletion* onto Docker contexts; fixes the documented product failure. **Independent of §0** — detach keys off the durable `Session.id`. | remotes + dx |
| 2 | **Approval loop — reframed to a signal *producer*; ✓ done for the single-player automated context.** `AttentionRequired{NeedsInput}` on transcript `end_turn` → log/`subscribe`/webhook; front-ends respond via `session send`. In-pillbox `approve\|deny\|answer` verbs reframed away; the mid-tool blocked sub-signal is closed as **not hook-viable** (Notification suppressed in the automated context — reading vt100 is the only reliable path if ever wanted). | dx + event-log |
| 3 | **§0 LOCAL SUBSTRATE — ✓ DONE + live-verified** — `sessionId` + durable per-session log + co-located sequencer + zero-config `Subscribe(from_seq)` (`notify` tail) + a producer. *Re-scoped: `actor` / event-system-merge / network-seq → multiplayer; cross-sandbox `Session` → migration; `class` → pooling — NOT §0.* | The keystone. The substrate's local stream **every** consumer reads — inner-loop readout, fleet triage, lum, *and* (later) multiplayer. | event-log + gateway + dx |
| 4 | **Reader bits — ✓ DONE** — `session watch` (thin human reader, `docker logs` model); **`session list` status-from-log + `session diagnose`** (status = fold the events.jsonl terminal sink + per-session log → starting/running/needs-input/done/failed; honest about host-visible reach). | Falls out of §0 (the log). Makes a swarm triageable + the cheapest "watch your agent" — a *consumer over the public tap, not a UI*. | dx |
| 5 | **Harden #1/#3** (transcript+MITM source of truth; native secondary). #2 `raw_body` deferred | Cheap; off the critical path. | harden |
| 6 | **Multiplayer web-attach** (the multi-human circulation demo) — needs `Frame` seq/ack + bounded `DataAck` + WS endpoint + `session share` | **Demoted** below the cheap local-visibility + approval work: the §0 spine already gives single-player "watch," and the share-a-link demo is heavier. Still the multi-human artifact for orca / the gsv builder. | multiplayer |
| 7 | **Multiplayer input / roles / join links** | Governed multi-writer; defer until web-attach shows pull. | multiplayer |
| 8 | **Remotes: `k8s://` + managed tier** | Second transport + the paid bundle-as-a-service. | remotes |
| 9 | **Profiles (net-new typed object) + topologies** | Profiles = prerequisite for the (external) optimization layer; net-new, not "already there." | multiplayer + dx |

**Why this order.** Steps 1–2 need almost no new infrastructure (a deletion; a
new event + verbs over shipped transport), so they buy usability fast. Step 3 —
the **§0 keystone** — is the foundational substrate surface that generalizes
them onto the durable log *and* is the local stream lum/Slack/`pillbox watch`
subscribe to. Web-attach (step 6), the old "first circulation demo", **demotes**:
the §0 local subscribe surface + a thin reference reader already deliver "watch
your agent think" single-player — the cheaper demo and the inner-loop unblocker
— so the multi-human share-a-link demo follows once its heavier prerequisites
(seq/ack, bounded `DataAck`, WS, `share`) are in.

(Note: **"§0" names the event-log keystone throughout the spec set** —
gateway.md, session-event-log.md, remotes-redesign.md — sequenced 3rd here; the
"#" column is ordinal order, not a §-label.)

**Optimization is a parallel external track, not a pillbox step** — and per the
scope verdict, CUT from this repo. If pursued elsewhere it merely *consumes* the
contract (`Subscribe`/`from_seq` + outcome events); pillbox's only obligation is
to keep that contract solid.

## Built so far (2026-05-31)

Reconciled against the sequence above; commits on `main` (origin synced).

- **Step 1 — remotes / `docker://`:** foreground run-assembly, tar-cp
  secret-denylist ingest, result extraction, sandbox-side vault, the
  blob-scrub security fix — **done + live-verified**. Open: `docker://`
  `--detach`, version-skew, pull-progress.
- **Step 3 — §0 (local substrate): DONE + live-verified** (and re-scoped — the
  structural bits moved out of §0, see "§0 is the local substrate" above).
  - durable per-session log (`events/log.rs::SessionLog` →
    `<pillbox>/sessions/<id>/log.jsonl`), per-session monotonic seq (the log is
    the **co-located single-writer sequencer**, recovered on open), `read_from`
    replay, `subscribe(from,stop,sink)` (`notify` tail), `Payload::Unknown`;
    `sessionId` on the Event; the first producer (transcripts→log, always-on,
    **lossless** — `Usage`/`Thinking`/`model`/`stop_reason` in proto + Rust); the
    zero-config local subscribe surface as a **WS gateway** (`session subscribe`).
  - *Re-scoped OUT of §0 (land with their drivers):* `actor` + (b) event-system
    merge + network seq-authority → **multiplayer**; (c) cross-sandbox `Session`
    → **migration**; `class` → **pooling**. No current single-player consumer.
- **Ahead of sequence (Step 7 input / dx):** the **drive surface** — `session
  send` (SendInput → pty-host) + **tail-while-serving** (`session subscribe` on
  a live session tails its transcript→log while serving) — closes the
  drive+read loop on one detached, interactive (subscription-billed, not `-p`)
  session, **live-verified**. Plus agent **pre-trust + `--permission-mode
  auto`** (local + remote) so seeded/driven interactive doesn't stall on the
  trust dialog / per-tool prompts. Initial-prompt seeding works via the agent's
  positional prompt (`run -- "prompt"`), no code.
- **Step 2 — approval loop, reframed + first piece done:** it's a signal
  **producer**, not per-tool gating or in-pillbox UX. pillbox emits
  `AttentionRequired{ NeedsInput }` on the transcript's explicit `stop_reason ==
  "end_turn"` → the durable log / `subscribe` stream, for a front-end (orca /
  lum / Slack) to flash / seek-input; respond via `session send`. Live-verified.
  Remaining: the mid-tool **blocked/permission** signal — **researched then
  empirically falsified (2026-05-31).** The doc-based plan (Claude Code's
  **`Notification` hook**, `notification_type` ∈ idle_prompt/permission_prompt/
  elicitation_dialog) was built as a producer (pre-seed a hook into the
  bind-mounted home; fold the marker into `AttentionRequired`) and **probed
  against a real detached session before wiring the consumer.** Findings:
  - **`Notification` hooks do NOT fire** in pillbox's detached/automated
    pty-host context. `idle_prompt` no-showed after 80s idle (it should fire
    immediately if notifications were live); `permission_prompt` no-showed in
    gating mode (`--permission-mode default`) on a tool-using prompt. Claude's
    `Notification` channel is the *"ding a human at a terminal"* mechanism and
    is suppressed when there's no interactive user — exactly pillbox's case.
  - **Lifecycle hooks DO fire** (`Stop` wrote its marker in the same detached
    session) — but only via a **`~/.claude/settings.json` file**; inline
    **`--settings` hooks are ignored in the interactive TUI** (they fire under
    `claude -p`, which misled the isolated test). And `Stop` == turn-end ==
    **redundant with the shipped `end_turn`→NeedsInput signal**, so it buys
    nothing.
  - **Verdict:** reverted the non-firing producer (the `--settings` plumbing).
    `end_turn`→NeedsInput stays as *the* attention signal. The mid-tool
    blocked/permission signal is an **open limitation in the automated context**
    — Claude exposes no hook that fires there for it. `StopFailure` (error-stall
    → `ErrorStalled`) is lifecycle-class so it *may* fire, but it's untested
    (needs a real API error to trigger) and niche; revisit only if the
    error-stall case proves worth a dedicated probe. **Not OSC** either (Claude
    emits no native idle/permission OSC; orca's OSC 9999 is itself hook-authored,
    so it would hit the same suppression).
  - **If the blocked signal is ever wanted:** the reliable path is **reading the
    PTY**, not a hook. In gating mode (`--permission-mode default`) the block is
    visible — the permission dialog paints on the terminal and claude idles for a
    y/n keystroke — and pillbox already snapshots vt100 for the drive surface. So
    a blocked signal, if pursued, is a vt100-content detector, not a hook.
  - **Status:** Step 2 is therefore **as complete as it can be for the
    single-player automated context** — the producer ships and fans out (below);
    the in-pillbox `approve|deny|answer` verbs were reframed away (front-ends
    respond via `session send`); the blocked sub-signal is closed as not
    hook-viable. No remaining hook work.
- **Fan-out architecture (decided 2026-05-31, after an adversarial review).**
  "One signal → all subscription types" is realized as **the per-session log is
  the bus; every consumer is a read-side tailer of it** — NOT a producer-side
  push bus + lifecycle-`EventType`-fold (reviewed and rejected: the file is the
  cross-process IPC even in-process, so push buys nothing the detached path can
  use; network sinks on the append path would stall the agent; and the fold
  drags in the deferred host↔sandbox seq handoff — see the gateway spec). Built:
  `subscribe` now uses a **`notify` tail** (file stays the single bus, every
  reader improves); a **read-side webhook exporter** (`events::
  spawn_webhook_log_exporter`) tails the log and POSTs `AttentionRequired` to
  `$PILLBOX_EVENTS_WEBHOOK` off the producer's path. **Live-verified:** one
  `attention_required` produced once fans to both the WS `subscribe` stream and
  the webhook. Remaining on this track: make `events.jsonl` a read-side
  **projection** of the per-session logs; **defer** the `EventType`→`Payload`
  fold + routing lifecycle through the seq authority until multiplayer needs the
  unified actor/seq thread *and* the seq handoff is designed.
- **Step 4 — reader bits, started:** **`session watch`** ships (the thin
  human-facing reader — renders the event stream to the terminal, the `docker
  logs` model; `subscribe` is the machine/WS sibling). Remaining: **`session
  diagnose`** (collector-free post-mortem from the log) + **`session list`
  status-from-log** (running / idle / done, not just attached/detached).
- **Not started:** Steps 5–9 (Harden #1/#3, multiplayer web-attach, input/roles,
  k8s/managed, profiles).

**Net:** §0 *as the usable local substrate* (watch + drive your agent, no
collector) is **done and re-scoped to that**. The structural pieces once lumped
into §0 — `actor`, the event-system merge, cross-sandbox `Session`, gateway
authority — are **not** §0; they land with their drivers (multiplayer /
migration / pooling) and are deliberately unbuilt until then.

## Cross-cutting decisions (the genuine forks)

- **Input arbitration** — driver-token vs turn-queue. Default:
  **driver-token + a queue for programmatic inputs**. (event-log §Open 4,
  multiplayer.)
- **Gateway placement for remote** — host-side sequencer first (simpler,
  one authority); sandbox-side provisional seq reconciled later if host
  disconnects bite. (event-log §Open 1.)
- **Global `events.jsonl`** — keep as a lifecycle-only projection of the
  per-session logs; source of truth moves per-session. (event-log §Open 2.)
- **Managed detach = billing while detached** — needs an idle-timeout /
  default TTL policy on the paid tier; reuse `--ttl` / `session prune`.
  (remotes §Open.)
- **Collective-intelligence governance** — default: open consumption,
  opt-in contribution, quality-gated inclusion; *never* required-to-consume.
  Open knob: what the contributor carrot is (lean: premium routing on the
  managed tier). (optimization §Data principles.)
- **Optimization metric / reward channel** — adopt **Harbor as the eval
  *interface*** (Terminal-Bench 2.0 / SWE-bench / DeepSWE are datasets run
  *through* it, not alternatives). Optimize against contamination-resistant sets
  (SWE-rebench date-gated); SWE-bench Verified is sanity-only (contaminated). The
  **verifiable reward is a *substrate primitive* pillbox must ship** — an
  external grader scoring the rustic **result-snapshot + exit code** — and it
  **gates the entire compile-time loop**. Never the self-reported
  `session.completed` (`session.rs`). See [swarm-memory.md](./swarm-memory.md).
- **Vault egress is a correctness gap, not a feature** — the proxy passes
  *non-matched* hosts through unmodified (`vault/server.rs:6`), so an agent can
  POST any other secret to an arbitrary host. Add strict-deny egress (403 on
  unmatched) before/with the managed tier. Infisical Agent Vault + Cloudflare
  Sandbox Outbound both ship this — convergence that *reinforces* "bundle is the
  moat, not the vault."

## What this is NOT

- Not a feature-moat play — the bundle is the moat (or the acquihire
  artifact); individual features are commodity.
- Not a competitor to managed-sandbox vendors on price — managed pillbox
  sells the bundle on commodity compute, not cheaper sandboxes.
- Not an extractive data play — local capture is sovereign; pooling is
  opt-in and scrubbed (signal not code; artifacts not demos). The day
  pillbox collects silently, trust — its only moat — becomes its biggest
  liability.
