# `pillbox dispatch` — the worker-loop primitive

**Status: shipped (GHOST-002 contract → GHOST-003 loop → GHOST-004 live e2e).**
This documents the CLI surface, the verdict JSON schema, and the exit codes. The
fork/score/select loop is implemented in `src/commands/dispatch.rs` and
live-verified by `scripts/smoke/dispatch.sh`. **libkrun-only today** (the grader
resolves each worker's live workspace via `session info --json` →
`.session.workspace`, libkrun-only; docker workspace resolution is deferred — see
"Deferred" below). Downstream programs against the contract on this page — change
it here first.

> **Two axes (H4 + the enumerated control, `docs/optimization-gate.md`):** the σ̂
> experiments found the *segmentation* lever is in-session focused-prompt chaining +
> per-checkpoint verification (gating adds a real +0.18 on top of the decomposed
> prompt) — now shipped as **`--segments`** (ONE session per worker, the proven
> lever). `dispatch`'s fork-`k` is the separate **best-of-k diversity** axis; they
> compose (`--segments -k N`). Two caveats still open: fork-`k`'s own efficacy as a
> diversity lever isn't measured yet (the smoke validates plumbing, not outcome),
> and both results are single-model (glm-5.1) pending H5 (cross-model).

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
| `--rubric FILE` | — | Grader: a rubric file (`NAME :: COMMAND` per line, `#`/blank lines ignored) → per-criterion verdicts + a fractional score. Mutually exclusive with `--cmd`. Same format `session score --rubric` parses. In `--segments` mode this stays the **final reward** (the gates are per-segment). |
| `--segments SPEC` | — | Drive an ordered **segment chain** (TOML, below) in ONE session per worker — the proven in-session segmentation lever (`docs/optimization-gate.md` §2026-06-19) — instead of one prompt. Composes with `-k` (best-of-k over chains). See **Segments** below. |
| `--retries N` | `1` | Per-worker retry budget when the grade fails — the failing criteria are fed back as the next prompt and the worker is re-graded, up to `N` times. With `--segments`, this is the **per-segment** gate-retry budget. |
| `--agent AGENT` | pillbox `agent =`, then `claude` | Worker agent (`claude` \| `codex` \| `opencode` \| …). |
| `--model MODEL` | agent default | Worker model override, forwarded to each worker's run. |
| `--temperature FLOAT` | agent default | Per-fork sampling temperature, forwarded to each worker — the diversity knob that keeps best-of-`k` non-degenerate. |
| `--workers-spec FILE` | — | A **heterogeneous worker roster** (TOML, below) — one `[[worker]]` row per fork binding that worker's agent/model/temperature. The roster length is the authoritative `k`. See **Workers spec** below. |
| `--memory` | off | Wire in kypp swarm-memory (`--memory`) for each worker (scoped briefing + post-run capture). |
| `--ttl DURATION` | — | Per-worker retention TTL (`30m`/`24h`/`7d`), forwarded to every forked worker. Losers are left **running** (not auto-killed) so their §0 evidence stays readable; a TTL is how a dispatch campaign reaps them via `session prune` instead of leaking `k` VMs per run. See **Cleanup** below. |
| `--json` | off | Emit the verdict as JSON on stdout instead of the human banner. |
| `-- <PROMPT>…` | — | The task prompt handed to every worker (trailing args). **Required** in fork-`k` mode; **optional** in `--segments` mode (the segments carry the work — given, it's context prepended to segment 1). |

The grader is a required, mutually-exclusive group: exactly one of `--cmd` /
`--rubric` must be given (a clap `ArgGroup` enforces it → a missing/both case is
a usage error, exit 2). This holds in `--segments` mode too — the reward is always
required, distinct from the per-segment gates.

## Segments (`--segments`)

The proven segmentation lever: drive focused, checkpoint-gated **sub-prompts
sequentially in ONE session** (context accumulates, the horizon never resets).
H4 + the enumerated control (`docs/optimization-gate.md`) showed this — *not*
fork-per-segment — is what cuts σ̂ and lifts the mean; `-k` fork stays the
separate **best-of-k diversity** axis, and the two compose (`--segments -k N` runs
`N` independent chains, selects the best by the final reward).

The spec is TOML — an ordered list of `[[segment]]`, each a focused sub-prompt
(`prompt` xor `prompt_file`) and a **gate** (`gate_rubric` xor `gate_cmd`).
Relative paths resolve against the spec file's directory; unknown keys are a parse
error.

```toml
[[segment]]
name        = "reroot"
prompt_file = "segments/01-reroot.txt"   # or: prompt = "..."
gate_rubric = "segments/01-reroot.rubric" # or: gate_cmd = "pytest -k reroot"

[[segment]]
name   = "pathfind"
prompt = "Implement Tree.path_to(from_node, to_node) ..."
gate_cmd = "python3 -m pytest -k path"
```

Per worker, each segment is: `send` its prompt → wait-idle → grade against its
**gate** → on a failed gate with budget left, re-drive with the distilled summary
(`--retries` per segment). **A failed gate does not abort the chain** — it advances
and lets the run-level `--rubric`/`--cmd` **reward** be the authoritative final
grade (the gate only steers progression; the reward selects the winner). The
worker's `retries_used` is the sum across segments.

Gates are **self-contained** — they run against the worker's live workspace as-is
(same as `dispatch --rubric`). This is the boundary from the σ̂ eval harness
(`scripts/eval/segmentation/`), which injects *hidden* test subsets at grade time;
`--segments` is for real work whose tests live in the workspace.

## Workers spec (`--workers-spec`)

By default `dispatch -k N` forks `N` identical workers (same agent/model; only
`--temperature` varies). `--workers-spec FILE` makes the `k` workers a
**heterogeneous roster** — each `[[worker]]` row binds the `i`-th fork's
`agent` / `model` / `temperature`. The TOML is an ordered list of `[[worker]]`,
all fields optional:

```toml
[[worker]]
agent = "claude"
model = "anthropic/claude-opus-4-8"

[[worker]]
agent = "opencode"
model = "zai-coding-plan/glm-5.2"
temperature = 0.7
```

The roster length is the authoritative **`k`** — you don't pass `-k`. (An
explicit `-k N` is allowed only when it *matches* the roster length; a
disagreeing `-k` is a usage error, exit 2.) Per field, the precedence is
**per-worker row → run-level `--agent`/`--model`/`--temperature` →
`pillbox.toml`**: an omitted row field falls back to the run-level flag, which
itself falls back to the pillbox descriptor. Unknown keys are a parse error
(exit 2, before any fork), and an empty roster is rejected. Omit `--workers-spec`
entirely and behavior is byte-identical to today's homogeneous fork-`k`. The
roster composes with `--segments` (each rostered worker runs the chain). See
`rubrics/example-workers.toml`.

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
        "retries_used": 0,       // retries this worker consumed (sum across segments in --segments mode)
        "status": "scored",      // "scored" | "failed" | "errored" (see below)
        // ADDITIVE — present ONLY for a --segments worker; omitted in fork-k mode.
        // The per-checkpoint trajectory, in order; `score` is the gate score.
        "segments": [
          { "name": "reroot",   "passed": true, "score": 1.0, "retries_used": 0 },
          { "name": "pathfind", "passed": true, "score": 1.0, "retries_used": 1 }
        ]
      },
      { "session": "def456...", "score": 0.5, "passed": false, "retries_used": 1, "status": "failed" },
      { "session": "ghi789...", "score": null, "passed": false, "retries_used": 0, "status": "errored" }
    ],
    // Host directory the winner's result workspace was pulled to (a durable temp
    // staging dir, `$TMPDIR/pillbox-dispatch-<run>/winner-<id>`), or null when
    // there is no winner.
    "pulled_to": "/tmp/pillbox-dispatch-019eef4f.../winner-abc123def456",
    // Why the winner was selected — its score + the tie-break that decided it,
    // tied to the verifier output. Null when no worker passed.
    "selection_rationale": "only passing worker (score 1.00)"
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

## Worker evidence (the `dispatch.worker_summary` artifact)

The `--json` verdict is the *summary*; the durable, mineable evidence is a §0
artifact. After grading, dispatch writes **one `dispatch.worker_summary`
[artifact](./session-event-log.md#payload-taxonomy) per worker, on that worker's
own session log** — co-located with its trajectory, so a debugger or a later
self-harness pass can answer *why* a worker passed or failed, not just read the
scalar score. The log line is a small typed reference; the body (the full
evidence JSON) lives in the worker session's content-addressed blob store.

Read a worker's evidence:

```sh
pillbox session log <worker-id> --type artifact   # the reference (kind, summary, blobRef)
pillbox session artifact get <worker-id> --ref <blobRef>   # the full evidence body
```

The body schema:

```jsonc
{
  "session": "<worker id>",
  "status": "scored",            // scored | failed | errored
  "passed": true,
  "score": 1.0,
  "retries_used": 1,
  "prompt": "<the segment prompt this worker was driven with>",
  "winner": true,
  "selection_rationale": "only passing worker (score 1.00)",  // winner only
  "grader": "rubric:r.txt",      // absent for an errored worker
  "criteria": [ { "name": "tests", "passed": true, "feedback": "5 passed" } ],
  "feedback": "…the grader's combined output…",
  "judge_report_ref": null       // GHOST-011 hook (below)
}
```

Failed and errored workers get a summary too (errored ones omit the grade
fields), so a losing attempt's evidence isn't lost. Writing is **best-effort**:
a log-write hiccup warns to stderr and is skipped — it never changes the
dispatch outcome (the run already succeeded; evidence is observability, not
correctness). The artifact is stamped `actor: svc:dispatch` (the orchestrator,
not the worker agent).

**`judge_report_ref` — the GHOST-011 hook (advisory, opt-in, not built here).**
The slot is always present (null today) so the schema is forward-compatible.
When the cross-vendor judge / Fusion lane lands, it attaches a `judge.report`
artifact and points this ref at it — purely *advisory* (a Goodhart guard / a
critique of the winner), **never** a selection input: the execution-grounded
verifier decides the winner, a judge panel never overrides it.

## Exit codes

Consistent with the pillbox exit-code table (`CLAUDE.md`):

| Code | Meaning |
|---|---|
| `0` | A winner was selected (at least one worker passed) and its workspace pulled. |
| `1` | No winner — every worker either failed its grade (`failed`) or never produced a gradeable result (`errored`). The exit code is deliberately coarse: to tell an infra break (all `errored`) from a legitimate all-fail (all `failed`), read the per-worker `status` in the `--json` envelope, not the exit code. |
| `2` | Usage error — `-k 0`, neither/both of `--cmd`/`--rubric`, or a bad flag. |

> **Note:** exit `0` reports the winner even if its *pull* failed — `pulled_to`
> is then `null` and a recovery hint (`pillbox session pull <id>`) rides stderr.
> A `--json` consumer should check `pulled_to`, not just the exit code.

## Cleanup (worker teardown)

dispatch does **not** auto-kill workers. After a run, the winner and every loser
are still live sessions. This is deliberate: each worker's evidence is a
`dispatch.worker_summary` §0 artifact on **its own** session log, and `session
rm` removes the session *record* — which orphans that log from the read surface
(`session log` resolves the record). Auto-rm'ing losers would free the VMs but
make their evidence unreadable, and `session rm` does **not** reap per-session
krun state (creds/workspace/sock) either.

So the cleanup model is:

- **One-off:** inspect losers (`session log <id> --type artifact`, `session
  pull <id>`), then `session rm` each when done.
- **Campaign:** pass `--ttl` so every worker gets an `expires_at`, and run
  `pillbox session prune` (cron/orchestrator) to reap expired sessions — the
  standard retention path, evidence-safe until prune.

Known gap (tracked, not dispatch-specific): `session rm`/`prune` leave
`~/.pillbox/krun/{creds,ws,*.sock}` on disk; over a long campaign the
accumulation degrades fresh-VM launches. The eval harness works around this with
its own `reap_session`; the proper fix belongs in `session rm` itself.

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
  segments. v1 pulls to a durable temp staging dir
  (`$TMPDIR/pillbox-dispatch-<run>/winner-<id>`), which the caller reads back from
  `pulled_to` (outside cwd, so it's never swept into a commit; reaped with `$TMPDIR`).
- **Docker-backend dispatch** — v1 is **libkrun-only**: the grader resolves each
  worker's *live* workspace via `session info --json` → `.session.workspace`,
  which only libkrun sessions populate. A docker run needs a non-libkrun
  workspace-resolution (pull-then-score, or score the `result_snapshot` after
  `session done`). The loop itself is backend-agnostic; only the grade step is
  coupled. (`scripts/smoke/dispatch.sh` skips non-libkrun backends.)
- **`files_changed` in the worker summary** — a per-worker diff-vs-base summary
  (the touched paths / a stat) is a natural evidence field, but needs a
  workspace-vs-`from_bookmark` diff op (rustic/git). The mining-critical evidence
  (grade breakdown, retries, prompt, rationale) ships now; `files_changed` is an
  additive field on the artifact body when the diff op lands.

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
