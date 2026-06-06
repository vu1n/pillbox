# Developer experience — the bundle is the product

> **Scope.** pillbox is **local-only**: two local backends — Docker (default,
> cross-platform) and libkrun (a local microVM, opt-in via
> `PILLBOX_BACKEND=libkrun`, see [libkrun-sandbox.md](./libkrun-sandbox.md)).
> This doc covers the local-first inner loops and the drive/§0 surface. "Remote"
> returns later as a managed/Cloudflare tier with a different shape; mentions of
> it here are forward-looking, not current.

Status: design / proposed. Sibling to [vnext.md](./vnext.md).

vNext's own thesis: **there is no single-feature moat, so the integrated
bundle is the product** — and the *developer experience* of that bundle is the
circulation/acquihire artifact. That promotes DX from "polish" to "the
deliverable." This doc is the DX contract the other specs are measured against.

## The one principle

> **Every load-bearing journey must have a zero-config *local* path. OTLP
> collectors, managed backends, and team infra are opt-in *graduation* —
> never prerequisites.**

**Substrate, not UI.** This does *not* mean pillbox grows a UI — the proto's
own contract is "consumers own their threads, identity, and UIs." It means the
substrate must **expose its streams locally and zero-config** — the PTY *and*
the structured event stream — so any consumer (lum, a Slack thread, an IDE, a
CI bot, or a thin first-party reference reader) can subscribe without standing
up a collector. The gap today: the PTY is a zero-config local tap, but the
structured stream is **OTLP-only**, so the thing a consumer subscribes to needs
a collector. A first-party `pillbox watch` is allowed *only* as a **reference
consumer** over the same public `Subscribe` contract everyone else uses (the
`docker logs` / `git log` model) — never a privileged path. Local == public
parity, or it's the lock-in we're avoiding.

pillbox has built the hard substrate (per-session records, transcript→span
synth, vault MITM, content-addressed rustic store, the `PillboxError` +
exit-code contract, a transport-agnostic frame protocol, the coalesced
Input-frame channel). What's missing is the **local-facing sinks and verbs**
that turn that substrate into a loop a human can use. Today the tool is
*slow-and-blind by default* on the journeys the docs sell hardest. The
fixes are overwhelmingly **additive** — a local sink, three events, a profile
object, a few CLI verbs — not a rewrite.

## The journeys (with acceptance criteria)

### 1. Optimization inner-loop — "run → see → tweak → re-run"

Acceptance: a solo dev, **no collector**, can run an agent, watch it live,
inspect what it did afterward, change one thing, re-run, and **diff the two
runs**.

- **Zero-config local *subscribe surface*** (the substrate's job — not a UI).
  The TranscriptEvents are already parsed harness-agnostically but only an OTLP
  sink consumes them (`emit_event_span` no-ops without a tracer), so the stream
  a consumer would subscribe to isn't locally available without a collector.
  Fix: expose the structured stream **locally + zero-config** —
  `Subscribe(from_seq)` over a local socket/WS (the event-log §0 surface) +
  persist it as JSONL under the state dir. **Rendering is a consumer's job**
  (lum, a Slack thread, an IDE). Ship a *thin optional reference reader*
  (`pillbox watch`) that consumes only that public tap — the `docker logs`
  model: a default reader, not a UI product. local==public parity, same schema.
  (Arize Phoenix / Workshop are *consumers* of the stream, not what pillbox
  becomes.)
- **Every run is a diffable experiment.** Auto-snapshot cwd into the existing
  rustic repo per local run (base→result), key
  each run by `(workspace snapshot + frozen profile version)`, and ship
  `pillbox session diff A B` (config delta + trace/result delta). Today
  `session diff` is a deferred comment (`session.rs:170`); local runs don't
  snapshot.
- **Profiles** (see below) are the unit you tweak.
- *Fast:* image pull gets real progress; warm the cold container via a rustic
  prebuild base + per-host pre-warm; when the deferred `raw_body` store lands,
  expose single-LLM-call replay (the highest-leverage iteration primitive).

### 2. Detached / fleet triage — "1 human → N agents"

Acceptance: with ~10 detached sessions, a dev can see **which need me /
which failed / which are done** at a glance, and **answer a blocked agent
without reattaching a PTY**.

- **`session.blocked` is a first-class lifecycle event** (alongside started/
  completed/failed/dropped — `events/mod.rs` has only those four today).
  Emitted the instant a gate is hit, through the **existing sinks** (jsonl /
  webhook / OTLP — zero new transport). Payload: session id, reason/category,
  pending action + args, TTL, default-on-timeout action.
- **Out-of-band decision verbs:** `pillbox session approve|deny|answer ID
  [--text …]`, plus a Slack/webhook actionable callback. The decision injects
  over the **already-shipped coalesced Input-frame channel** (commit d590a2d) —
  it constructs a `PermissionResolved` (the type exists in `contract.rs`; it
  has **zero producers** today) and pushes it down. Reference: Cursor / Devin /
  Jules / Copilot suspend→notify→resume.
- **Reversible vs. irreversible gates.** Reversible → auto-resume on TTL
  (compose with the existing `--ttl`/`prune` machinery), logged. Irreversible
  (push / merge / result-finalize) → hard-block; the agent cannot self-approve
  even under `--dangerously-skip-permissions`. This is a security boundary, not
  just ergonomics.
- **`session list` shows status.** Today it renders `attached_pid` (PTY
  ownership) only and never reads `events.jsonl`, so done/failed/blocked/
  running are visually identical. Join against the log: a status + exit-code +
  needs-attention column, `--status` / `--needs-attention` / `--failed`
  filters, status-first ordering, a `--watch` mode. Print the
  already-captured-but-unprinted fields (`status`, `exit_code`, reason,
  `result_snapshot`) in `session info`. Reference: `fly status`.

## The workspace rule: the *interaction model* picks the strategy (no flag)

A foreground run binds cwd live; a detached run should not trap the agent's
edits where the user can't see them. Resolve it by interaction model —
**automatically, no flag**:

- **Interactive `pillbox run`** → **live workspace** (bind-mounted cwd): edits
  appear in the user's editor as the agent makes them, results are already in
  cwd on exit.
- **`--detach` / autonomous / fleet** → **snapshot-fork** (overlay-CoW over the
  rustic base): results land via snapshot→pull; this is where fork-from-store
  earns its keep (fan out N agents from one base).

Liveness and materialization are orthogonal layers, not competing models: the
live workspace is the interactive overlay *on top of* the same container + vault
+ attach stack; the on-exit snapshot still captures cwd→store (host-side
authority). The user never picks; the mode follows interactive-vs-`--detach`.

Remote — a managed/Cloudflare tier with a different shape — returns later; this
doc's contracts are scoped to the local backends.

## The profile primitive (currently 0% surface)

vNext leans on "pillbox already has profiles" for Aquifer-parity and the
compounding-moat story (meta-harness tunes profiles; sharing scrubbed profiles
is the circulation play). But `rg -i profile src/{cli,config}.rs` → **0 hits**:
no `[profile]` table, no `--profile`, no store. v0.6 even dropped the v0.5
per-project run-defaults (`with`/`mount`/`env`), leaving "wrap it in a shell
alias" — which keeps the recurring-injection ergonomics *out* of the bundle
that's supposed to travel. Without a typed profile, the external optimization
project has nothing to consume or emit, and a diffable config delta is
impossible.

Ship a concrete **versioned, frozen profile object** — `agent + runner image +
mounts + env/secret NAME-refs + context policy + model` — with `pillbox profile
create|edit|export|import|diff` and `pillbox run --profile NAME`. Home: a
`[profile]` / `[run]`-defaults table in `pillbox.toml` (also re-fixes the
dropped run-defaults) + a content-addressed, pinnable store (reuse rustic
content-addressing), composable via the existing global→project inheritance.
**Share the NAME manifest only, never plaintext secrets.** Stamp every session
record with the exact profile version it ran under so `session diff` shows a
clean config delta.

## Latent wins the plan under-sells

- **`pillbox session diagnose ID`** — the event log + transcript synth +
  content-addressed snapshots already are a complete, attributed, replayable
  record. Once `session.blocked` + local transcript persistence land, a
  one-command collector-free post-mortem ("failure reason + last N tool calls +
  pending gate + `pull this snapshot to reproduce`") falls out for free. The
  plan calls this plumbing; it's a flagship triage feature for the swarm
  operator.
- **The approval loop is a thin verb over shipped transport** (the d590a2d
  Input-frame channel) — not net-new broker work.
- **The local subscribe surface is mostly §0** — the parser already produces
  harness-agnostic events; exposing them as a local zero-config `Subscribe` tap
  (what lum/Slack/`pillbox watch` all read) is the event-log §0 surface, not a
  separate UI build. The substrate exposes; consumers render.
- **`--ttl`/`prune` is the graceful-degradation engine** for blocked-gate
  timeouts.
- **`PillboxError` + stable exit codes** are a ready-made machine-readable
  diagnostic API (Coder's named-error-code analogue) — good DX is mostly
  *generalizing the existing contract*, not a new error system.

## Quick wins (cheap, high-value)

- Local-run pre-handoff banner: `pillbox: running claude against myapp (docker)
  — /workspace/myapp, vault: off`. Closes the silent mis-targeting footgun
  (~10 lines).
- Give `HumanSink`/`JsonlSink` real arms for the three HITL payloads (today
  `_ => {}`) so even an attached terminal shows "agent is blocked: <reason>".
- `pillbox session events --session ID` per-session projection of the firehose.
- Print `status`/`exit_code`/reason/`result_snapshot` in `session info` human
  output (in `to_json_value` today, not printed).
- **CI test that every `Next:` command in an error string is a real runnable
  pillbox command** (Coder #16468 lesson) — guard the affordance from rotting.
- Surface the Workshop/observability nudge to **all** fresh users, not only
  those who already have `~/.raindrop` on disk.

## Reference designs (the field has proven these — adopt, don't invent)

| Journey moment | Adopt from |
|---|---|
| Zero-config local viewer | Arize Phoenix, Raindrop Workshop |
| Suspend → notify → resume approval | Cursor / Devin / Jules / Copilot |
| One-link read-only-prefix sharing, roster, request-control | sshx / tmate / VS Code Live Share |
| Preflight, graded version-skew, pull-progress | Coder / DevPod / Daytona |
| state + status view | `fly status` |
