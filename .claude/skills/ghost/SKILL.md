---
name: ghost
description: Run ghost (the conductor) on a REAL task in the current repo — snapshot cwd, author an execution-grounded reward, dispatch verified worker loops, show the diff, apply on accept, reap. This is the dogfooding front door for the meta-harness: you (the interactive seat) ARE the conductor. Use when the user says `/ghost <task>` or asks to "have ghost do" a self-contained, verifiable unit of work. Libkrun-only. The CLI (`ghost run`) is deliberately unbuilt — its shape is being derived from this skill's friction log.
---

# `/ghost <task>` — the conductor, on real work

`pillbox dispatch` is the delegation **primitive** (contract: `docs/dispatch.md`,
when-to-use: the `dispatch` skill). **This skill is the conductor *gesture* on top
of it** — the one-move workflow that hides the snapshot→reward→dispatch→apply→reap
ceremony so ghost is something the user can actually reach for on a real task.

You are the conductor. The durable job the design hands you (`scripts/ghost/DECISIONS.md`)
is **authoring the reward and selecting the result** — *not* writing the agent's code.
That is what dissolves the "rubric wall": the user does not hand you a rubric, you
derive one from the task.

> **Phase note:** the `ghost run` CLI is intentionally not built. We dogfood through
> this skill first so the CLI's shape is *derived from real friction*, not guessed.
> Every run ends by appending one line to `scripts/ghost/DX-LOG.md` (step 6). That
> log is the accreting CLI spec — treat the capture step as load-bearing, not optional.

## 0. Fit check — dispatch, or do it inline? (be honest)

Dispatch only pays off when **all** hold (the `dispatch` skill's gate):

- **Verifiable** — you can name a command or rubric that objectively passes/fails it
  (tests green, build clean, a script exits 0, an invariant holds). No machine-checkable
  signal → no reward → **do not dispatch**.
- **Self-contained** — the work lives in a workspace you can snapshot.
- **Variance or decomposition matters** — luck (→ fork-k) or a real sequential split
  (→ segments). A one-shot trivial edit doesn't need a fan-out.

If the task is exploratory, conversational, needs your judgment mid-loop, or has no
grader: **say so and do the work inline.** Dogfooding ghost by misusing it on
unverifiable work teaches the wrong lesson. The first good dogfood targets are
*recurring, test-checkable* units: "make this failing test pass", "fix this lint
across module X", a refactor whose suite must stay green.

## 1. Prereqs (fail loud, with the fix)

- **libkrun backend only.** `PILLBOX_BACKEND` must be `libkrun` (the default). On
  anything else dispatch isn't wired — do the work inline.
- Codesigned libkrun binary (`scripts/lk-build.sh`), the worker agent authed
  (`pillbox auth login --agent <opencode|claude|…>`), the runner image present.
- A **project** pillbox (`pillbox.toml` in/above cwd) — `push --bookmark` needs one.
  If cwd is bare, `pillbox new --name <repo>` first (ask before creating).

If a prereq is missing, stop and surface the exact fix. Don't limp.

## 2. Snapshot cwd → bookmark (hidden ceremony)

Workers fork from a snapshot bookmark, so snapshot the *current* working tree:

```sh
pillbox push --bookmark "ghost-<short-slug>" --json    # slug from the task
```

`push --bookmark` snapshots cwd and names it atomically. This captures the working
tree as-is (including uncommitted work), so the workers start from exactly what the
user sees. Keep the slug task-derived so concurrent `/ghost` runs don't collide.

## 3. Author the reward (your core act — pick or write)

Exactly one of `--cmd` / `--rubric` is **required** (a missing/doubled grader is a
usage error). Prefer the **cheapest execution check that can't be faked**:

- **Single objective check → `--cmd`.** One verifier via `sh -c`, exit 0 = pass.
  Best for "make X pass": `--cmd 'cargo test -p foo bar::'`, `--cmd './repro.sh'`,
  `--cmd 'cargo clippy --all-targets -- -D warnings'`.
- **Multi-criterion → `--rubric`.** A `NAME :: COMMAND`-per-line file → a fractional
  score (a real gradient). The palette is the starting point — copy + specialize, do
  not run a generic rubric and pretend it graded the task:
  - `rubrics/rust-change.rubric` — fmt + clippy + build + test (a Rust change).
  - `rubrics/test-pass.rubric` — build + suite green (language-agnostic skeleton).
  - `rubrics/repro-script.rubric` — a repro script exists, is `+x`, exits 0.
  - `rubrics/doc-change.rubric` — a doc exists + holds its link/marker invariants.

**Forge-resistance (the sharp edge).** The grader runs against each worker's *own,
editable* workspace — so a check that lives **inside** it is gameable: a worker can
weaken a committed test, or `touch` the file your `test -f` looks for. A forge-resistant
reward either (a) checks state the task may not mutate, or (b) is **injected from
outside the snapshot** at grade time — a host-side `--cmd` referencing a hidden grader
by absolute path that the worker never sees (the way the σ̂ eval harness does). Reach
for (b) whenever "done" can't be pinned to immutable state.

Reward discipline (`DECISIONS.md` GD-003): the reward is the **forge-resistant
selector** and it is *yours* — author it from the task **before** you see any worker's
output, never from a model's opinion of its own work where a real verifier exists.
`session score` execution, never self-report.

## 4. Dispatch (pick the axis from the task shape)

```sh
pillbox dispatch --from-bookmark "ghost-<slug>" \
  (--cmd '<verifier>' | --rubric <file>) \
  --agent opencode [--model M] \
  [-k 3 --temperature 0.7] [--segments spec.toml] \
  --ttl 24h --json -- "<the task>"
```

- **Default `-k 1`** for the first dogfood of a task — cheapest, proves the loop. Raise
  to `-k 3 --temperature 0.7` only when run-to-run **variance** matters (without the
  temperature the forks are degenerate and best-of-k buys nothing). If all `k` score
  identically, `k` was too high — the task had no variance to exploit and you paid `k`×
  for one answer.
- **`--segments spec.toml`** when the task has a genuine sequential decomposition
  ("do A, verify A, then B, verify B") — the proven σ̂-cutter. Compose `--segments -k N`
  when it *also* has variance. Authoring a spec: see the `dispatch` skill / `docs/dispatch.md`.
- **`--ttl 24h`** always, so the workers are reapable via `session prune` instead of
  leaking k VMs (step 6).

## 5. Read the verdict + apply (winner up, not transcripts)

Consume the `--json` envelope, never the fan-out transcripts:

```jsonc
{ "version": 1, "dispatch": {
  "winner": "abc123…",                  // null ⇒ no worker passed
  "workers": [ { "session": "…", "score": 1.0, "status": "scored", "segments": [...] } ],
  "pulled_to": "$TMPDIR/pillbox-dispatch-<run>/winner-abc123…",  // temp staging dir — CHECK THIS
  "selection_rationale": "only passing worker (score 1.00)" } }
```

- **Exit 1 / `winner: null`** — no worker passed. Read per-worker `status` to tell an
  all-`failed` (legit hard task / wrong reward) from all-`errored` (infra). Report it
  honestly; don't paper over a no-winner as a win. This is a **pivot trigger** (step 5b),
  not a silent retry.
- **Exit 0** — check `pulled_to` is non-null (a null means the *pull* failed though a
  winner was found; recover with `pillbox session pull <winner>`).

**Apply the result — surgically.** `pulled_to` is a durable **temp staging dir**
(`$TMPDIR/pillbox-dispatch-<run>/winner-<id>`) — a full copy of the snapshot tree +
the agent's edits, *outside* your repo (so it is never swept into a commit, but it
lives under `$TMPDIR` — apply what you want before it's reaped). Diff it against your
working tree and apply **only the task's changed files**:

```sh
diff -ru "$PULLED" .        # what the winner changed vs your tree (ignore __pycache__/ etc.)
# then copy the specific changed files into your repo deliberately, file by file
```

**Never bulk-copy the staging dir over your repo** — it would clobber foreign
uncommitted WIP in this shared tree (the scarred-history rule: only touch files this
task changed).

**Tighten the winner for consistency (required after a fanout).** The reward is
*functional, not stylistic* — a winner can pass every test yet clash with house idiom,
and a **heterogeneous roster** (different models picking different winners,
`--workers-spec`) fragments style across merges. So before applying, run **`/tighten`
on the winner's diff** with explicit attention to consistency-with-neighbors — naming,
structure, idiom, comment density (the "reads like the surrounding code" bar) — not
just signal-per-token. And bias *selection* toward catching it up front: include
`fmt --check` + lint gates in the reward where they exist (e.g.
`rubrics/rust-change.rubric`'s `fmt`/`clippy` lines), so the winner isn't chosen
style-blind. fmt/lint don't catch idiom, so the post-fanout `/tighten` is the backstop,
not the whole defense.

### 5b. Pivot, never merge (GD-004)

If the winner is wrong or thin, **do not stitch multiple workers together** (merging
disjoint contexts is banned by design). Pivot: refine the reward or the prompt and
re-dispatch from the same bookmark, or pick a different rostered worker
(`--workers-spec`) and re-run. Selection + pivot, one coherent lineage at a time.

## 6. Reap + capture friction (both required)

**Reap.** Workers are left running (their §0 evidence stays readable). With `--ttl`
set, `pillbox session prune` reaps them; for a one-off, read any loser evidence you
want (`session log <id> --type artifact`) then `pillbox session rm <id>` each, and
`pillbox bookmark rm ghost-<slug>` when done. `session list`/`rm`/`prune` resolve a
project's workers **only from inside the project dir** — cd there first (or pass
`--pillbox NAME`), or they'll report nothing.

**Capture friction (the meta-goal of this phase).** Append **one line** to
`scripts/ghost/DX-LOG.md` (create it if absent) recording what was awkward this run —
the bookmark-naming, the reward-authoring, the manual diff/apply, the reaping, a flag
you wished existed. This log is how the `ghost run` CLI spec accretes from real use
instead of being guessed. Format:

```
- <date> · <task one-liner> · k=<n> <cmd|rubric> · won=<y/n> · FRICTION: <the one thing that should have been a flag/default>
```

A `/ghost` run that doesn't leave a friction line wasted its main purpose.
