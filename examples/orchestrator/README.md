# Tiny orchestrator — productive-failure-loop spike

A ~100-LOC bash + jq script that drives the Open-Inspect-style loop on
top of pillbox v0.7's lifecycle events. **This is a spike**, not a
product — its job is to validate that pillbox's event schema is
usable from a dumb consumer. If bash + jq can drive it, anyone can.

## The loop (per the Open-Inspect diagram)

```
1. Ambitious task ──▶ 2. End-to-end run ──▶ 3. Failure (as expected)
       ▲                                              │
       │                                              ▼
5. Upgrade the factory ◀── 4. Second-pass analysis
   (human, out of scope)        (analyzer agent, in scope)
```

What this script handles: stages 1-4. Stage 5 (humans editing prompts /
tooling based on the analyzer's report) is deliberately out of scope —
that's an iteration step, not an orchestration step.

## Two scripts

- **`smoke.sh`** — minimum runnable test against the v0.7 spike. Spawns a session, observes `session.started`, drops it, observes `session.dropped`. ~60 LOC. Validates the event transport works.
- **`run.sh`** — the full productive-failure loop. **Specification only today** — needs PR 1's `session pull` and PR 2's `session.completed`/`session.failed` events to run end-to-end. Use it as the design target for what we're building toward.

## Usage

```sh
# Smoke test (runnable today against the spike)
PILLBOX_REMOTE=prod-cloud ./smoke.sh

# Full productive-failure loop (runnable after PR 1 + PR 2)
PILLBOX_REMOTE=prod-cloud ./run.sh "Refactor the auth module to use the new session API"
```

## What pillbox API surface the orchestrator needs

This drives PR 2 of v0.7. If the spike works against this surface,
PR 2 ships these as stable.

| Call | What it returns | Why we need it |
|---|---|---|
| `pillbox run --remote NAME --detach --json -- AGENT_PROMPT` | `{"session_id": "abc123def456", ...}` on stdout | Spawn the ambitious-task session and capture its id |
| `pillbox session events --follow [--filter session_id=X] [--json]` | JSONL stream on stdout, one event per line | Subscribe to lifecycle transitions for a specific session |
| `pillbox session pull <id> --to DIR` | Rehydrates the failed fork's workspace to DIR | Give the analyzer agent the failed run's state to read |
| `pillbox session rm <id>` | (existing) | Clean up after the loop completes |

## What pillbox events the orchestrator subscribes to

| Event | When emitted | Payload fields |
|---|---|---|
| `session.started` | After the sandbox + PTY are up and the agent has been launched | `session_id`, `parent_session_id?`, `agent_id`, `remote`, `backend`, `started_at` |
| `session.completed` | Agent finished successfully (or `pillbox session done <id>`) | `session_id`, `started_at`, `ended_at`, `status: "ok"`, `trace_path?` |
| `session.failed` | Agent exited non-zero, sandbox died, or `pillbox session done <id> --status=failed` | `session_id`, `started_at`, `ended_at`, `status: "error"`, `reason`, `trace_path?` |
| `session.dropped` | `pillbox session rm <id>` — sandbox killed, record removed | `session_id`, `at` |

OTel-shaped field names from day one — `session_id` becomes the
OTel `span_id`, `parent_session_id` becomes `parent_span_id`,
`started_at`/`ended_at` become span timing.

## Open questions the spike will answer

1. **Is one event stream enough, or do we need per-session streams?** Today's sketch tails the global stream and filters by `session_id`. If filtering is awkward in practice (high-cardinality, slow), we may want `pillbox session events SESSION_ID --follow` as a per-session shortcut.
2. **What `reason` shape does `session.failed` need?** Free-text string? Structured enum (`exit-non-zero`, `oom-killed`, `network-error`, `helper-crashed`, `user-cancelled`)? Probably the latter for orchestrator dispatch, with a free-text `message` for humans.
3. **What goes in `trace_path`?** The agent's tool-call JSONL? Just stdout/stderr? Both? Format TBD — likely a rustic snapshot reference that `session pull` can rehydrate alongside the workspace.
4. **Should the orchestrator be able to spawn an analyzer session that bind-mounts the failed fork**, or does it always go through `session pull` + a workspace mount? The latter is simpler; the former saves a copy.
5. **What happens if pillbox is upgraded mid-run?** Events emitted by an older pillbox; consumer expects a newer schema. Versioning story TBD.

## Out of scope for the spike

- Durability across orchestrator process death (use Absurd for that experiment, in a separate `examples/orchestrator-absurd/`).
- DAGs / multi-step dependencies.
- Concurrent task fan-out.
- Capability registry / factory-upgrade automation.
- Anything that requires pillbox to grow workflow-engine surface.

## When to throw this away

When PR 2 lands, this script either:
- (a) Becomes the canonical "how to consume pillbox events" example, kept in `examples/orchestrator/`.
- (b) Spins out as the seed of a real orchestrator product (separate repo).
- (c) Gets deleted because we built a richer example in its place.

For now, treat it as a load-bearing test, not production code.
