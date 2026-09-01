---
name: dispatch
description: Orchestrate verified worker loops with `pillbox dispatch` — fork-k best-of-k diversity and/or in-session segment chains, selected by an execution-grounded reward. Use when deciding whether to delegate a task to forked workers vs work inline, how to author a segment spec, and how to read a verdict without drowning in transcripts. Libkrun-only.
---

# Dispatching verified worker loops

You are the **interactive seat** (a chat orchestrator). `pillbox dispatch` is your
delegation primitive: it forks detached worker sessions from a snapshot bookmark,
drives each to idle, **grades each with an execution-grounded reward**, retries the
ones that fail, then **selects the highest-scoring worker** and pulls its result
workspace. You consume the verdict (winner + per-segment outcomes), not the
transcripts. Contract: `docs/dispatch.md` — program against that, never invent flags.

**Libkrun-only today** (`PILLBOX_BACKEND=libkrun`). On any other backend dispatch is
not wired — do the work inline instead.

## When to dispatch vs work inline

Dispatch when **all** hold:

- The task is **verifiable** — you can write a command or rubric that passes/fails it
  objectively (tests green, a build clean, a script exits 0, an invariant holds). A
  task with no machine-checkable success signal has no reward → don't dispatch it.
- The work is **self-contained** in a workspace you can snapshot to a bookmark.
- Either variance/luck matters (fork-k), or the task has a real sequential
  decomposition (segments), or both.

Work **inline** when the task is exploratory, conversational, needs your judgment in
the loop, or has no objective grader. Dispatch is for *closing* a well-posed unit of
work, not for thinking out loud.

## The two axes (the load-bearing distinction)

Dispatch has **two orthogonal levers**. They are not the same thing; keep them
straight.

| Axis | Flag | What it buys | Use when |
|---|---|---|---|
| **diversity** (best-of-k) | `-k N` + `--temperature F` | `k` independent attempts at one prompt, select the best | one horizon, but **variance/luck** matters — pick the lucky run |
| **segmentation** | `--segments SPEC` | drive focused, checkpoint-gated sub-prompts **sequentially in ONE session** | the task has a genuine **sequential decomposition** |

- **fork-k is diversity, not decomposition.** `k` workers each attempt the *whole*
  task; you keep the best. With identical deterministic workers all `k` score the
  same and best-of-k buys nothing — so pair `-k` with `--temperature` (e.g. `0.7`)
  to make the attempts diverge.
- **`--segments` is the proven variance-cutter + mean-lifter** (the σ̂ experiments).
  Context accumulates across segments; the horizon never resets. This is **NOT**
  fork-per-segment — it is one session walking a checkpoint-gated chain. If a task
  naturally splits into "do A, verify A, then do B, verify B," segment it.
- **They compose.** `--segments -k N` runs `N` independent segmented chains and
  selects the best chain by the final reward — best-of-k over decomposed work. Reach
  for this when a decomposed task *also* has run-to-run variance.

Default mental model: **if the task decomposes, segment it; if it's one horizon with
luck variance, fork it; if both, compose.**

## The Goodhart line (do not conflate gate and reward)

There are two distinct grading channels — keep them separate or the loop games itself:

- **Per-segment gate** (`gate_rubric` / `gate_cmd` in the spec) — only **steers
  progression** within a chain and feeds distilled retry feedback. A failed gate does
  **not** abort the chain; it advances. The gate is a checkpoint, not the verdict.
- **Run-level reward** (`--rubric` / `--cmd` on the dispatch command) — the
  **authoritative, forge-resistant selector**. This picks the winner. It is required
  in every mode, including `--segments`.

Reward = `session score` (execution-grounded), **never self-report**. An agent saying
"done" is not a pass. Never let a gate substitute for the reward, and never let the
reward be a model's opinion of its own work where a real verifier exists.

## Authoring a segment spec

The spec is TOML — an ordered list of `[[segment]]`, each a focused sub-prompt and a
gate. See `rubrics/example-segments.toml` for the canonical copy-me example.

```toml
[[segment]]
name        = "reroot"
prompt_file = "segments/01-reroot.txt"      # xor: prompt = "..."
gate_rubric = "segments/01-reroot.rubric"   # xor: gate_cmd = "python3 -m pytest -k reroot"

[[segment]]
name     = "pathfind"
prompt   = "Implement Tree.path_to(from, to) so it returns the node list."
gate_cmd = "python3 -m pytest -k path"
```

Rules (from the contract):

- Each segment needs `prompt` **xor** `prompt_file`, and `gate_rubric` **xor**
  `gate_cmd`. Unknown keys are a parse error.
- **Relative paths resolve against the spec file's directory** — keep prompt/rubric
  files next to the spec.
- **Gates are self-contained** — they run against the worker's live workspace as-is.
  Write each gate to test only what *this* segment produced (`pytest -k <segment>`,
  not the whole suite), so a checkpoint fails for the right reason.
- Segments are **ordered and cumulative** — segment N may rely on N-1's output. Order
  them so each gate is checkable the moment that segment finishes.

## Sensible defaults

- **`-k 3`** for short horizons (k=3–5 saturates; diversity gains flatten beyond).
  `-k 1` (the default) = pure segmentation with no diversity.
- **`--retries 1`** (the default) — one distilled retry per failing gate. Raise only
  for flaky/hard checkpoints; it costs a full re-drive each.
- **`--temperature 0.7`** whenever `-k > 1` — without it the forks are degenerate.
- **Reward is always required** — give exactly one of `--cmd` / `--rubric` (a missing
  or doubled grader is a usage error, exit 2).

## Consume the verdict, not the transcripts

Run with `--json` and read the envelope (full schema in `docs/dispatch.md`):

```sh
pillbox dispatch --from-bookmark base --segments spec.toml -k 3 --temperature 0.7 \
  --rubric rubrics/rust-change.rubric --agent opencode --json \
  -- "Refactor the parser into staged passes."
```

```jsonc
{ "version": 1, "dispatch": {
  "winner": "abc123…",                 // selected worker, or null if none passed
  "workers": [ { "session": "abc123…", "score": 1.0, "passed": true,
                 "retries_used": 1, "status": "scored",
                 "segments": [ {"name":"reroot","passed":true,"score":1.0} ] } ],
  "pulled_to": "/path/to/session-abc123…",   // winner workspace; check this, not just exit code
  "selection_rationale": "only passing worker (score 1.00)" } }
```

- Read **winner + per-worker `score`/`status`/`segments`** — the trajectory stays in
  each worker's §0 log so your context doesn't drown in fan-out.
- **Check `pulled_to`**, not just the exit code: exit `0` reports the winner even if
  its *pull* failed (then `pulled_to` is null; recover with `pillbox session pull`).
- Exit codes: `0` = a winner was selected and pulled; `1` = no winner (read per-worker
  `status` to tell all-`failed` from all-`errored`); `2` = usage error.
- Need *why* a worker passed/failed? Each worker writes a `dispatch.worker_summary`
  §0 artifact on its **own** log:
  `pillbox session log <worker-id> --type artifact` →
  `pillbox session artifact get <worker-id> --ref <blobRef>`.

## Cleanup (workers are left running)

dispatch does **not** auto-kill workers — the winner and every loser stay live so
their §0 evidence stays readable. Reap them yourself:

- **One-off:** inspect losers, then `pillbox session rm <id>` each when done.
- **Campaign:** pass `--ttl 24h` (`30m`/`24h`/`7d`) so every worker gets an
  `expires_at`, then `pillbox session prune` (cron/orchestrator) reaps the expired
  ones — evidence-safe until prune.

Don't `session rm` a loser before you've read its evidence — `rm` removes the record,
which orphans its log from the read surface.
