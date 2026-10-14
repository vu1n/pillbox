# `pillbox dispatch` — the worker-loop primitive

**Status: shipped (GHOST-002 contract → GHOST-003 loop → GHOST-004 live e2e).**
This documents the CLI surface, the verdict JSON schema, and the exit codes. The
fork/score/select loop is implemented in `src/commands/dispatch.rs` and
live-verified by `scripts/smoke/dispatch.sh`. **libkrun-only** (every grade runs in
the grader microVM; a build without the `libkrun` feature refuses to dispatch).
Downstream programs against the contract on this page — change it here first.

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
workspace. Grading never touches the worker's live tree and never runs on the
host — see **Grading** below. The verdict schema is unchanged.

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
| `--stall-limit N` | `2` | **Non-convergence breaker.** Stop re-driving a worker after `N` retries in a row that don't raise its best `--rubric` score, even with `--retries` budget left: it is going in circles (the same failures, or a new one for each one it fixes). Applies per segment with `--segments`. `--cmd` grades are 0 until they pass, so they carry no progress signal and keep the plain budget. Never fires at the default `--retries 1`; it matters once the budget is raised. `0` disables it. A worker the breaker stopped carries `"stalled": true`, independent of its `status`: in fork-`k` mode it ends `failed`, but with `--segments` the chain moves on past a stalled gate and the final reward can still pass. |
| `--baseline-check` | off | Before forking, restore `--from-bookmark` into a fresh temp dir per grader (so one grader's build output can't leak into the next) and grade it on the host with the reward and every segment gate. The base is the trusted bookmark, so this needs no microVM. A gate has to **fail** on the untouched base to mean anything (the dispatch form of "a new test fails without the change"). A reward that already passes there stops the run with exit 2 before any worker boots; a segment gate that passes only warns. The result rides in the verdict as `baseline`. |
| `--protect PATTERN` | — | Extra protected path, repeatable, added to the defaults. Restored from the bookmark in the grade tree (see **Grading**). A name matches at any depth (`fixtures/`, `*.golden`); a pattern containing `/` is anchored at the workspace root (`ci/run.sh`, `/Makefile`). |
| `--no-default-protect` | off | Drop the default protected set; only `--protect` patterns apply. For a task whose job is to change the tests. |
| `--grader-egress HOST` | — | Repeatable. Let the grader microVM reach these hosts (a package registry the tests install from). The grader is offline by default — the same fence as `session score --grader-egress`. |
| `--grader-files DIR` | — | Hidden grader files. `DIR` is copied into every grade tree (and the `--baseline-check` base) at `.pillbox-grader/`, locked, and never into a worker's workspace, so a worker can neither read nor edit them. Reference them from the reward: `--cmd 'sh .pillbox-grader/check.sh'`. `.pillbox-grader/` is protected, so a worker's own copy is removed. Symlinks inside `DIR` are copied as their targets' content; one that leaves `DIR` or dangles is an error. |
| `--restate-task` | off | Re-send the positional task with every retry turn and ahead of every segment after the first. Off, the task rides only the first turn: a retry carries only the distilled failure summary, and with `--segments` only segment 1 sees the task. On, each of those turns adds the task **verbatim** (byte-identical) inside a delimited block:<br>`The original task, restated verbatim:`<br>`<original-task>`<br>`…the task…`<br>`</original-task>`<br>If the task itself contains `<original-task>` or `</original-task>`, the tag becomes `<original-task-1>`, then `<original-task-2>`, and so on, until neither tag occurs in the task, so the whole task stays inside the block. A retry turn is the failure summary followed by the block (a segment retry adds the segment's prompt in a `<current-segment>` block after it, delimited the same way); a later segment is the block followed by its prompt. The first turn is unchanged (fork-`k` turn 1 *is* the task; segment 1 already has it prepended), so the task is never sent twice in one turn. Needs a positional prompt (exit 2 without one, including whitespace-only). Agents with no server profile (`claude`, `claude-stream`, `codex`, `cursor`, `pi`) never take a second turn, so the flag changes nothing for them; server agents (`opencode`, `codex-serve`) do. |
| `--agent AGENT` | pillbox `agent =`, then `claude` | Worker agent (`claude` \| `codex` \| `opencode` \| …). |
| `--model MODEL` | agent default | Worker model override, forwarded to each worker's run. |
| `--temperature FLOAT` | agent default | Per-fork sampling temperature, forwarded to each worker — the diversity knob that keeps best-of-`k` non-degenerate. |
| `--workers-spec FILE` | — | A **heterogeneous worker roster** (TOML, below) — one `[[worker]]` row per fork binding that worker's agent/model/temperature. The roster length is the authoritative `k`. See **Workers spec** below. |
| `--memory` | off | Wire in kypp swarm-memory (`--memory`) for each worker (scoped briefing + post-run capture). |
| `--ttl DURATION` | — | Per-worker retention TTL (`30m`/`24h`/`7d`), forwarded to every forked worker. Losers are left **running** (not auto-killed) so their §0 evidence stays readable; a TTL is how a dispatch campaign reaps them via `session prune` instead of leaking `k` VMs per run. See **Cleanup** below. |
| `--json` | off | Emit the verdict as JSON on stdout instead of the human banner. |
| `--critic KIND` | — | Score each worker's edits with a calibrated **System One critic** before the verifier runs (`typesafe` = TypeSafe `jev-latest`; needs `TYPESAFE_API_KEY`). The verifier stays the reward; the critic's probability is recorded next to it. See **Critic** below. |
| `--critic-policy POLICY` | `record` | What the probabilities do: `record` (measure only, grading unchanged) \| `order` (grade in descending `p`, stop at the first passer) \| `select` (grade ONLY the argmax-`p` worker). Requires `--critic`; `order`/`select` are fork-`k` only. |
| `--critic-model MODEL` | `jev-latest` | The critic's model id. |
| `-- <PROMPT>…` | — | The task prompt handed to every worker (trailing args). **Required** in fork-`k` mode; **optional** in `--segments` mode (the segments carry the work — given, it's context prepended to segment 1, and to every later segment and retry with `--restate-task`). |

The grader is a required, mutually-exclusive group: exactly one of `--cmd` /
`--rubric` must be given (a clap `ArgGroup` enforces it → a missing/both case is
a usage error, exit 2). This holds in `--segments` mode too — the reward is always
required, distinct from the per-segment gates.

## Grading

A worker is graded on a **copy** of its workspace, never the live tree, and the
grader runs in the one-shot libkrun microVM (`session score --in-sandbox`), never
on the host. The copy is secret-scrubbed with the same denylist as a worker's
clone. The VM starts with `env_clear` and only the grader's static env (`HOME`,
`TERM`, `PATH`) — no host environment — and with no network unless
`--grader-egress` names hosts. `--baseline-check` is the exception: it grades
the trusted bookmark on the host, so it runs where a microVM can't boot.

**Protected paths** are put back from the bookmark before the grader runs, so a
worker cannot change its reward by editing, deleting, renaming, or symlink-swapping
them. The defaults cover test trees and test files (`tests/`, `test/`, `spec/`,
`__tests__/`, `testdata/`, `*_test.go`, `*_test.rs`, `test_*.py`, `*_test.py`,
`*.test.*`, `*.spec.*`), test-runner config (`conftest.py`, `pytest.ini`,
`tox.ini`, `.cargo/`, `jest.config.*`, `vitest.config.*`), root-level modules that
would shadow a Python test runner (`/pytest.py`, `/pytest/`, `/_pytest/`,
`/unittest.py`, `/unittest/`, `sitecustomize.py`, `usercustomize.py` — `python3 -m
pytest` puts the workspace first on `sys.path`), CI config (`.github/`, `.gitlab-ci.yml`,
`.circleci/`, `.buildkite/`, `.travis.yml`, `azure-pipelines.yml`, `Jenkinsfile`),
and rubrics (`rubrics/`, `*.rubric`). A name matches at any depth; `--protect`
adds more. A bookmark symlink inside the set that points at an unprotected path
elsewhere in the tree (`tests/unit -> ../shared-tests`) is graded as the
bookmark's copy of its target, so link to code under test by import path, not by
symlink. A protected symlink that is dangling or leaves the bookmark (absolute,
or climbing above the root) errors the grade: the VM mounts the tree elsewhere
and could resolve it to worker files. Manifests the task itself is often supposed to edit — `Cargo.toml`,
`package.json`, `pyproject.toml` — are not in the default set; a project that
grades through one of those adds it with `--protect`. When a grade reverts
something, dispatch names the paths on stderr; the verdict JSON is unchanged.

What protection does **not** cover, so write the reward around it:

- **Manifests can redirect a test run.** `autotests = false` in `Cargo.toml`
  stops cargo building `tests/`; `addopts`/`testpaths` in `pyproject.toml`
  redirect pytest; the `"test"` script in `package.json` replaces `npm test`.
  Name explicit targets in the reward (`cargo test --test parser`,
  `python3 -m pytest tests/test_parser.py`) so a vanished target fails, or
  `--protect` the manifest when the task doesn't need to edit it.
- **Tools resolved from the workspace** — `node_modules/.bin/*`, a `.venv` — are
  worker-controlled.
- **Worker code that runs during the grade** (`build.rs`, an earlier rubric
  criterion that runs project code) can leave state for later steps: files under
  `HOME=/tmp` (`$CARGO_HOME/config.toml`), build output, or a protected name the
  bookmark doesn't have (a new root `conftest.py`). Prefer one command that builds
  and tests together, or a `--grader-files` check that doesn't run project code
  first.
- **Inline tests** (`#[cfg(test)]` modules in source files) aren't paths.
- **Worker code the tests import runs inside the test process.** It can end
  that process with exit 0 before any assertion fails. Protection keeps the
  tests and the runner intact, not the process they share with the code under
  test; a reward that only runs the worker's program as a subprocess and checks
  its output is the stronger check.
- **A project that is itself a Python test runner or pytest plugin** (pytest,
  pluggy, coverage, hypothesis, `pytest-cov`…) is graded against the image's
  copy: the grader loads those before the workspace, so a worker can't swap
  them out.
- **`--grader-files` are hidden from the worker's agent, not from its code.**
  The reward runs as one user, so worker code it executes (an imported module,
  `build.rs`) can read `.pillbox-grader/` and print it, and failure feedback
  carries that output into the retry. Hidden checks stop an agent reading the
  answer key while it works, not a submission built to exfiltrate it. Dispatch
  refuses a `--grader-files` directory inside the project, which every worker's
  fork would already hold.
- **Tests the worker writes under a protected path are removed** before grading,
  like any other edit there. A reward or gate has to check tests that exist on
  the bookmark or come from `--grader-files`; a segment gate such as
  `pytest -k path` collects nothing if the segment's own new test is its only
  match. For a task whose job is writing the tests, use `--no-default-protect`.

Protected paths then lose every write bit, owner included, and the rest of the
copy becomes world-writable. Every directory on the way to a protected path
(the tree root, `src/` holding `foo_test.rs`) also gets the sticky bit, so code
running during the grade can still create files there but can't rename or
unlink the restored entries. The grader runs as `nobody` (`--drop-privileges`),
because the virtiofs server writes as the host user and a root guest could
`chmod` the lock off and let `build.rs` rewrite the tests before they compile.
Git's `safe.directory` is set to `*` for the grader, since `nobody` doesn't own the
mounted tree and git would otherwise refuse it as dubious ownership.
`PYTHONDONTWRITEBYTECODE` is set so pytest does not need to write `__pycache__`
into a locked `tests/`. `PYTHONSAFEPATH=1` plus a root-owned `sitecustomize.py`
first import the image's test runners (pytest, pluggy and the like), block
the ones the image lacks so a worker's `coverage.py` can't stand in for them,
and then
put the cwd (or script dir) after the stdlib but before site-packages, so a
worker's `pluggy.py` or `json.py` can't shadow the test runner or the stdlib,
while tests import the worker's project, even over a copy installed in the image. `--no-default-protect` with no `--protect` leaves the
tree alone and runs the grader as root, the opt-out.

The grade copy is made from a snapshot of the worker's workspace, frozen at
grade time and kept until the run ends. The winner delivered to `pulled_to` is
that snapshot (unscrubbed, protected paths as the worker left them, edits and
all), so a process the worker left running can't change the code between the
grade and the pull. Losers' snapshots are removed when dispatch exits.

A grade that never starts is an error, not a failed grade: the guest prints a
marker only after the workspace is mounted, and a missing marker (VM won't boot,
mount fails) fails that worker as `errored`. There is no host fallback. A build
without the `libkrun` feature is a usage error (exit 2) before any worker forks.

On macOS, the grade tree also drops every xattr from every entry, including
`user.containers.override_stat` and any `security.capability`, so a worker's
guest `chmod`, a root-owned setuid file or a file capability cannot outlive the
lock: the grader VM sees the locked backing modes (a file keeps the
exec bit the worker gave it).

**Each grade has a deadline** (default 1800s, `PILLBOX_GRADE_TIMEOUT` to
change it). A grader still running then is stopped and that worker is
`errored`, so one hung test suite can't stall the run. A grade that prints more
than 64 MiB is errored the same way.

**The grader VM boots the runner image**, which ships `python3` (no pytest),
`node`, `git` and core tools — **not** a Rust toolchain or pytest. A reward
whose tool is missing there (the first program the reward command names,
after setup builtins such as `cd` or `export`, is not found, exit 127, or a leading `python3 -m` of a runner tool such as pytest that is
not installed) is an
**error**, so the worker is `errored`, never
`failed`: otherwise every worker would "fail" identically and the run would
read as a hard task. A program missing *inside* the reward (`sh check.sh`
calling a `./bin/tool` the worker never built) is a failed grade. To grade with `cargo`, pytest or another toolchain, build
an image `FROM` the pillbox runner with it installed and set
`PILLBOX_RUNNER_IMAGE`; or install test deps inside the reward with
`--grader-egress` naming the registry. `--baseline-check` grades on the host,
so it can pass a reward the VM can't run; an `errored` worker after a clean
baseline usually means exactly that.

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
(`--retries` per segment; with `--restate-task`, that summary plus the restated
task and the segment's own prompt in a `<current-segment>` block, so the retry
points back at this checkpoint rather than the whole task). **A failed gate does not abort the chain** — it advances
and lets the run-level `--rubric`/`--cmd` **reward** be the authoritative final
grade (the gate only steers progression; the reward selects the winner). The
worker's `retries_used` is the sum across segments.

Gates are **self-contained** — they run against the worker's grade tree (the
scrubbed copy with protected paths restored; see **Grading**), same as the
run-level reward. This is the boundary from the σ̂ eval harness
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
        "score": 1.0,            // normalized score in [0,1] of this worker's
                                 // LAST graded attempt (not the best across
                                 // retries), or null if it never produced a
                                 // gradeable result (status "errored")
        "passed": true,          // did the grade pass (--cmd exit 0, or all rubric criteria)
        "retries_used": 0,       // retries this worker consumed (sum across segments in --segments mode)
        "status": "scored",      // "scored" | "failed" | "errored" | "unverified" (see below)
        "usage": {               // recorded worker usage snapshot; null when unavailable
          "input_tokens": 1200,
          "output_tokens": 300,
          "cache_read_input_tokens": 800,
          "cache_creation_input_tokens": null
        },
        // ADDITIVE — present ONLY when the --stall-limit breaker ended this
        // worker's retries early (in any segment). Independent of `status`:
        // fork-k → always "failed"; --segments → the chain continues past a
        // stalled gate, so the final reward may still pass ("scored").
        // "stalled": true,
        // ADDITIVE — present ONLY for a --segments worker; omitted in fork-k mode.
        // The per-checkpoint trajectory, in order; `score` is the gate score.
        "segments": [
          { "name": "reroot",   "passed": true, "score": 1.0, "retries_used": 0 },
          { "name": "pathfind", "passed": true, "score": 1.0, "retries_used": 1 }
          // a segment the breaker cut short also carries "stalled": true
        ],
        // ADDITIVE — present ONLY when --critic scored this worker (see Critic).
        "critic": { "critic": "typesafe", "model": "jev-latest", "p": 0.91,
                    "latency_ms": 180, "cost_usd": 0.00004, "input_tokens": 950,
                    "truncated": false, "n_edits": 2 }
      },
      { "session": "def456...", "score": 0.5, "passed": false, "retries_used": 1, "status": "failed", "usage": null },
      { "session": "ghi789...", "score": null, "passed": false, "retries_used": 0, "status": "errored", "usage": null }
    ],
    // Host directory the winner's result workspace was pulled to (a durable temp
    // staging dir, `$TMPDIR/pillbox-dispatch-<run>/winner-<id>`), or null when
    // there is no winner.
    "pulled_to": "/tmp/pillbox-dispatch-019eef4f.../winner-abc123def456",
    // Why the winner was selected — its score + the tie-break that decided it,
    // tied to the verifier output. Null when no worker passed.
    "selection_rationale": "only passing worker (score 1.00)",
    // ADDITIVE — present ONLY when --critic was given.
    "critic": { "kind": "typesafe", "policy": "order", "model": "jev-latest",
                "verifier_runs": 1, "verifier_runs_saved": 2 },
    // ADDITIVE — present ONLY with --baseline-check: how the reward and each
    // segment gate graded the untouched bookmark. The reward always failed here
    // (a passing one stops the run). `already_passing` lists rubric criteria that
    // pass before any work, so they can't separate workers.
    "baseline": {
      "reward": { "passed": false, "score": 0.5, "already_passing": ["builds"] },
      "segments": [ { "name": "reroot", "passed": false, "score": 0.0 } ]
    }
  }
}
```

`status` tokens (a wire contract):

| token | meaning |
|---|---|
| `scored` | Graded and **passed** (`--cmd` exit 0, or every `--rubric` criterion). |
| `failed` | Ran and was graded, but didn't pass after exhausting its retries. |
| `errored` | Never reached a gradeable result (boot / drive / score error); `score` is `null`. |
| `unverified` | Reached idle but was **never graded** — a `--critic-policy order` run found a passer earlier in critic order, or `select` picked another worker. `score` is `null`; `critic.p` is the only signal about it. Never a pass, never a winner. |

A worker that fails to fork, or whose turn doesn't go idle within the per-turn
timeout, becomes `errored` — one stuck/broken worker never sinks the batch (the
others are still driven and selected). The per-turn idle timeout defaults to
30 min; override with `PILLBOX_DISPATCH_TURN_TIMEOUT=<seconds>`.

### Recorded worker token usage

Every worker row and `dispatch.worker_summary` body has `usage`: either `null`
(no normalized usage events, no session, or an unreadable log), or an object
with the four nullable counters shown above. Dispatch reads each worker's own
session log **once at verdict finalization**, after selection and pull, and
copies that snapshot into its artifact. All recorded turns, retries, and segments
contribute regardless of `status`, including `errored` and `unverified` workers.
Log-read/write failures warn without changing grades, retries, selection, or exit
status. Critic `input_tokens` remains critic-only and is excluded from worker usage.

The fold shares `session cost`'s message identity and native-over-wire preference:
one observation per `message_id`, highest-ranked source wins, and equal-ranked
redeliveries keep the first. It sums normalized additive `Usage` events; it
doesn't sum raw cumulative session snapshots, legacy cost envelopes, or multiply
by retry counts. It never fills missing native fields from wire observations.
Each counter is a total only when **every selected observation reports that
field** and checked addition fits `u64`; otherwise that counter is `null`, even
if some events report a subtotal. Explicit reported zero stays `0`. Cache
counters stay separate from normalized non-cached input and output.

Which agents produce these events on a libkrun worker: `codex` (rollout
transcript), server agents such as `opencode` and `codex-serve` (event capture),
and `claude-stream` (its `result` line). The PTY `claude`, `pi` and `cursor`
record none yet, so their workers report `usage: null`.

These are **recorded observations**, not a claim of complete provider billing.
Unreported calls, skipped malformed log records, or producer lag cannot be
reconstructed. Workers may remain live and spend more after the snapshot;
dispatch doesn't stop them or wait for additional usage solely for accounting.
The legacy `session cost --json` schema and its zero-default behavior stay unchanged.

For fork-`k` and cross-model H5 validation, retain worker agent/model assignments,
the verdict, and the worker logs with each run. At partial and full budget
checkpoints, plot reward against cumulative **all-worker** tokens, including
losers and errors, using all four counters when the intended x-axis includes
cache tokens. Treat any required `null` as an unknown x-coordinate rather than
zero. These verdicts provide the finalization checkpoint; earlier checkpoints
require folding normalized event prefixes from the retained logs with the same
deduplication rule. When reading multiple snapshots of one session, use the
latest cumulative snapshot at the checkpoint; don't sum those snapshots. Sum
across independent worker sessions instead. Keep critic/grader spend separate,
record how token budgets/checkpoints were imposed externally, and report
observation gaps with the comparison. This instrumentation doesn't impose a
token budget or run the benchmark.

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
  "usage": { "input_tokens": 1200, "output_tokens": 300,
             "cache_read_input_tokens": 800, "cache_creation_input_tokens": null },
  "prompt": "<the segment prompt this worker was driven with>",
  "winner": true,
  "selection_rationale": "only passing worker (score 1.00)",  // winner only
  "grader": "rubric:r.txt",      // absent for an errored worker
  "criteria": [ { "name": "tests", "passed": true, "feedback": "5 passed" } ],
  "feedback": "…the grader's combined output…",
  "judge_report_ref": null,      // GHOST-011 hook (below)
  "critic": { "critic": "typesafe", "p": 0.91, … },  // only when --critic ran (see Critic)
  "stalled": true,               // only when the --stall-limit breaker fired
  "round_scores": [0.5, 0.5, 0.4]  // fork-k: the reward score of every graded attempt
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

## Critic (`--critic`)

The verifier is the reward — exact, exit-derived, unforgeable — and also the
expensive, slow, last step. With `-k` workers most verifier runs are spent on
losers. A **critic** is a cheap, calibrated System One judgment on a worker's
edits *before* the verifier runs: it returns a **probability** that the verifier
would pass, not a verdict. The verifier still decides every winner.

```sh
export TYPESAFE_API_KEY=…   # console.typesafe.ai/settings/keys
pillbox dispatch --from-bookmark seg-3 -k 4 --rubric grade.txt --agent opencode \
  --critic typesafe --critic-policy order -- "Implement the parser for segment 3."
```

What the critic sees is the worker's **edits from its own §0 log** (`session log
<id> --type tool_call`, completed `edit`/`write` calls) rendered with the task
prompt — the same representation the offline calibration harness scores, so an
offline Brier/ECE number is the right *starting estimate* for the live loop. It is
not a guarantee: live dispatches are a different distribution, which is why
`record` exists — it produces one labeled `(p, passed)` pair per run for ongoing
validation. Note the selection bias in the other two policies: `order`/`select`
only grade (and therefore only label) the workers the critic ranked highest, so
their pairs must not be pooled with `record`'s when measuring the critic. Shell writes
(`cat > f`, `sed -i`) are not recovered; that blind spot is the extractor's, and
it is stated rather than papered over. One `noul` question, phrased in
TypeSafe's own idiom; the probability comes back natively (no verbalized
confidence). The only critic today is `typesafe` (`jev-latest`); the seam is a
trait (`commands/critic.rs`).

Three policies:

| policy | what happens | what it's for |
|---|---|---|
| `record` (default) | Every worker is scored right before **each** of its grades; grading is unchanged. | Measurement. Every dispatch becomes an unbiased labeled `(p, passed)` pair — the only policy whose pairs are fit for measuring the critic. Composes with `--segments` (the critic judges the finished chain before the reward). |
| `order` | All forks are settled to idle first, then graded in **descending `p`**; the loop stops at the first passer. The rest are `unverified`. A worker that fails its first grade still gets its `--retries` before the loop moves on. | Saving verifier runs when the critic ranks well. `verifier_runs_saved` in the verdict is the count. |
| `select` | Settle all, score all, grade **only the argmax-`p`** worker. If it fails after retries, there is no winner (exit 1). | The "critic instead of verifier" claim, ground-truthed on the one worker it picked. The honest cost of trusting the critic is visible in the exit code. |

A critic failure (HTTP error, unreadable log) never sinks a worker: it is logged
to stderr and that worker simply carries no `critic` block (and ranks last under
`order`/`select`). A missing `TYPESAFE_API_KEY` is a usage error (exit 2) before
any VM boots. `order`/`select` with `--segments` is a usage error — a chain
grades once at the end, so there is nothing to reorder; use `record`.

Evidence: alongside the `dispatch.worker_summary` artifact (which now carries
`critic`), each scored worker gets a **`dispatch.critic_verdict` artifact of
class `signal`** — `p`, pass/fail, score, retries, policy; no prompt, no code —
so critic calibration can pool across runs without egressing content:

```sh
pillbox session log <worker-id> --type artifact | grep critic_verdict
pillbox session artifact get <worker-id> --ref <blobRef>
# { "session": "…", "critic": { "critic": "typesafe", "p": 0.91, … },
#   "policy": "order", "status": "scored", "verified": true, "passed": true,
#   "score": 1.0, "retries_used": 0 }
```

Without `--critic`, nothing above exists: the verdict, banner, and artifacts are
byte-identical to before.

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

- **`--to DIR` for the winner pull** — a deterministic output path for chaining
  segments. v1 pulls to a durable temp staging dir
  (`$TMPDIR/pillbox-dispatch-<run>/winner-<id>`), which the caller reads back from
  `pulled_to` (outside cwd, so it's never swept into a commit; reaped with `$TMPDIR`).
- **Docker-backend dispatch** — dispatch grades inside the libkrun grader
  microVM, so a docker build has nothing to grade with and `dispatch` refuses
  to start. (`scripts/smoke/dispatch.sh` skips non-libkrun backends.)
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
