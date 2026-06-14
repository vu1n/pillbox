# `pillbox dispatch` — the worker-loop primitive

**Status: contract only (GHOST-002).** This documents the CLI surface, the
verdict JSON schema, and the exit codes. The fork/score/select loop is
**GHOST-003**; the handler currently validates the invocation and reports
unimplemented. GHOST-003 and the live e2e (GHOST-004) program against the
contract on this page — change it here first.

## What it does (the shape)

Fork `k` detached worker sessions from a snapshot **bookmark** onto the same
segment prompt, drive each to idle, **grade** each with a `--cmd` or `--rubric`,
**retry** the ones that fail (feeding the failing criteria back as the next
prompt), then **select** the highest-scoring worker and **pull** its result
workspace.

It's the runtime half of ghost: the interactive seat (you, or a chat agent)
decomposes the work and calls `dispatch` per segment; the verb owns the
short-horizon, snapshot-anchored, rubric-verified inner loop. Best-of-`k` turns
long-horizon variance (σ̂) into expected gain rather than a measurement enemy —
which is why per-fork diversity (`--temperature`) matters: `k` identical
deterministic workers all score the same and select-best buys nothing.

Each worker is a `pillbox run --from-bookmark <name> --detach`, driven via the
existing `session send` / `wait-idle` / `score` / `pull` verbs — `dispatch` is
the loop that composes them, not a new sandbox path.

```sh
pillbox dispatch --from-bookmark seg-3 -k 3 --rubric grade.txt \
  --agent opencode --temperature 0.7 -- "Implement the parser for segment 3."
```

## Flags

| Flag | Default | Purpose |
|---|---|---|
| `--from-bookmark NAME` | — (required) | Snapshot bookmark every worker forks from — the shared base. |
| `-k`, `--workers N` | `3` | Number of parallel worker sessions to fork. Must be ≥ 1. |
| `--cmd CMD` | — | Grader: one verifier command via `sh -c` (exit 0 → pass / score 1.0). Mutually exclusive with `--rubric`; exactly one of the two is **required**. |
| `--rubric FILE` | — | Grader: a rubric file (`NAME :: COMMAND` per line, `#`/blank lines ignored) → per-criterion verdicts + a fractional score. Mutually exclusive with `--cmd`. Same format `session score --rubric` parses. |
| `--retries N` | `1` | Per-worker retry budget when the grade fails — the failing criteria are fed back as the next prompt and the worker is re-graded, up to `N` times. |
| `--agent AGENT` | pillbox `agent =`, then `claude` | Worker agent (`claude` \| `codex` \| `opencode` \| …). |
| `--model MODEL` | agent default | Worker model override, forwarded to each worker's run. |
| `--temperature FLOAT` | agent default | Per-fork sampling temperature, forwarded to each worker — the diversity knob that keeps best-of-`k` non-degenerate. |
| `--memory` | off | Wire in kypp swarm-memory (`--memory`) for each worker (scoped briefing + post-run capture). |
| `--json` | off | Emit the verdict as JSON on stdout instead of the human banner. |
| `-- <PROMPT>…` | — | The segment prompt handed to every worker (trailing args). |

The grader is a required, mutually-exclusive group: exactly one of `--cmd` /
`--rubric` must be given (a clap `ArgGroup` enforces it → a missing/both case is
a usage error, exit 2).

## Verdict JSON (`--json`)

The pinned envelope. Pin against `version: 1`; fields may be added in future
releases (the version bumps only on a restructure), matching the rest of
pillbox's `--json` surface.

```jsonc
{
  "version": 1,
  "dispatch": {
    // Session id of the selected (highest-scoring) worker, or null if none
    // produced a gradeable result.
    "winner": "abc123def456",
    // Every worker, in fork order.
    "workers": [
      {
        "session": "abc123def456",
        "score": 1.0,            // best normalized score in [0,1] across this
                                 // worker's attempts, or null if it never
                                 // produced a gradeable result (status "errored")
        "passed": true,          // did the grade pass (--cmd exit 0, or all rubric criteria)
        "retries_used": 0,       // retries this worker consumed
        "status": "scored"       // "scored" | "failed" | "errored" (see below)
      },
      { "session": "def456...", "score": 0.5, "passed": false, "retries_used": 1, "status": "failed" },
      { "session": "ghi789...", "score": null, "passed": false, "retries_used": 0, "status": "errored" }
    ],
    // Host directory the winner's result workspace was pulled to, or null when
    // there is no winner.
    "pulled_to": "/path/to/session-abc123def456"
  }
}
```

`status` tokens (a wire contract):

| token | meaning |
|---|---|
| `scored` | Graded and **passed** (`--cmd` exit 0, or every `--rubric` criterion). |
| `failed` | Ran and was graded, but didn't pass after exhausting its retries. |
| `errored` | Never reached a gradeable result (boot / drive / score error); `score` is `null`. |

A worker that fails to fork, or whose turn doesn't go idle within the per-turn
timeout, becomes `errored` — one stuck/broken worker never sinks the batch (the
others are still driven and selected). The per-turn idle timeout defaults to
30 min; override with `PILLBOX_DISPATCH_TURN_TIMEOUT=<seconds>`.

## Exit codes

Consistent with the pillbox exit-code table (`CLAUDE.md`):

| Code | Meaning |
|---|---|
| `0` | A winner was selected (at least one worker passed) and its workspace pulled. |
| `1` | No winner — every worker either failed its grade (`failed`) or never produced a gradeable result (`errored`). The exit code is deliberately coarse: to tell an infra break (all `errored`) from a legitimate all-fail (all `failed`), read the per-worker `status` in the `--json` envelope, not the exit code. (The contract stub also exits `1` today, with a "not yet implemented" message — transient, gone once GHOST-003 lands.) |
| `2` | Usage error — `-k 0`, neither/both of `--cmd`/`--rubric`, or a bad flag. |

## Deferred (additive, post-v1)

Out of the v1 contract on purpose — each is a new *optional* flag (+ optional
`DispatchOpts` field) that leaves the `--json` envelope unchanged, so adding it
later is additive, **not** a breaking contract change. Noted here so GHOST-003
treats them as deferred, not forgotten:

- **`--in-sandbox` / `--grader-egress HOST` passthrough to the grader** — for
  real-repo grading whose tests need the runner image's toolchain or to fetch
  deps (the libkrun grader path). v1 grades on the host (`session score`'s
  default); GHOST-003/004's gates don't need the sandboxed grader.
- **`--to DIR` for the winner pull** — a deterministic output path for chaining
  segments. v1 pulls to the default `./session-<id>`, which the caller reads
  back from `pulled_to`.
- **Docker-backend dispatch** — v1 is **libkrun-only**: the grader resolves each
  worker's *live* workspace via `session info --json` → `.session.workspace`,
  which only libkrun sessions populate. A docker run needs a non-libkrun
  workspace-resolution (pull-then-score, or score the `result_snapshot` after
  `session done`). The loop itself is backend-agnostic; only the grade step is
  coupled. (`scripts/smoke/dispatch.sh` skips non-libkrun backends.)

## Relationship to other verbs

`dispatch` composes existing primitives, it doesn't replace them:

- `run --from-bookmark --detach --json` — how each worker is forked.
- `session wait-idle` — how the loop knows a worker's turn is done.
- `session score --cmd|--rubric --json` — the **reward** channel each worker is
  graded on (the Goodhart-safe, non-self-reported verdict; `session done
  --status` is the agent's self-report and is *not* the gate).
- `session pull` — how the winner's result workspace is rehydrated.

The verdict reads worker **scores up**, not transcripts: the orchestrator
consumes `dispatch --json` and pulls only the winner; per-worker trajectories
stay in each session's §0 log for the learning loop, so a chat orchestrator's
context doesn't drown in fan-out.
