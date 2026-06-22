# ghost DX log — the accreting `ghost run` CLI spec

We are dogfooding ghost through the `/ghost` skill (`.claude/skills/ghost/SKILL.md`)
**before** building a `ghost run` CLI, so the CLI's shape is *derived from real
friction* instead of guessed (the same measure-before-optimize discipline that
killed the premature router).

Every `/ghost` run appends one line here recording the single thing that should have
been a flag or a default. When a friction recurs, it graduates into the CLI spec
(bottom). This file is the requirements doc, written by use.

## Friction log (one line per run)

Format: `- <date> · <task> · k=<n> <cmd|rubric> · won=<y/n> · FRICTION: <thing>`

- 2026-06-22 · env-parse (.env text → dict, 12 hidden cases) · k=3 t=0.7 · cmd=hidden grader · won=y (3/3 scored 1.0, 0 retries, ~3min, 0 orphans) · run #1, the gesture shakedown. Loop closed e2e by hand: snapshot→fork3→drive→grade→select→pull→verify→surgical-apply→reap. Findings F1–F5:
  - **F1 (confirms `auto-snapshot` hypothesis).** The pillbox repo isn't a project pillbox → had to `pillbox new` a throwaway first. ANY repo needs init before `/ghost` works. 1st confirmation.
  - **F2 (NEW → `--grader-file` hypothesis).** A *forge-resistant* reward is real work, not "pick a rubric": `--cmd`/`--rubric` grade against the worker's OWN editable workspace, so any in-workspace test is gameable. I hand-authored a hidden grader OUTSIDE the snapshot, invoked at grade time. The CLI likely wants `--grader-file F` (or `--grader-egress`-style injection) that's mounted at grade time, never snapshotted. (confirm: is out-of-workspace grading the common case or just for self-referential tasks?)
  - **F3 (skill+doc BUG, FIXED).** `pulled_to` is a durable `$TMPDIR/pillbox-dispatch-<run>/winner-<id>`, NOT `./session-<id>`. Skill step 5 + docs/dispatch.md both drifted; corrected in this change (code = `dispatch.rs:788-897`). Good outcome: outside cwd → never swept into a commit (the foreign-WIP scar is moot for the pull).
  - **F4 (NEW → observability).** `session list`/`session rm` only resolve a project's workers when cwd = the project dir — from elsewhere you see nothing. Reaping/inspecting requires cd into the project. (confirm: does `--pillbox NAME` already cover this? worth a `dispatch --json` field listing worker ids for cwd-independent reap.)
  - **F5 (sizing).** k=3 bought nothing here — all 3 passed identically. Best-of-k only pays when the task has real failure variance; on an easy task it's 3× cost for one answer. The conductor should size k to expected difficulty, not default to 3. (the `dispatch` skill already says k=1 default; the lesson is to RESIST reflexive k=3.)

## Graduated requirements (friction seen ≥2×, → CLI)

_Empty — nothing has recurred yet. The first `ghost run` flag earns its place here._

## Open hypotheses (what the CLI probably needs, to be confirmed by friction)

- **Auto-snapshot** — `push --bookmark ghost-<slug>` is pure ceremony; the CLI should
  snapshot cwd implicitly and clean the bookmark up after. (confirm: is the explicit
  bookmark ever *wanted*?) **[confirmed 1× — run #1 F1; also: target repo must be a
  project pillbox, so the CLI should `new`-on-the-fly or work against the global.]**
- **Reward inference** — detect the project's test/build command and default `--cmd`
  to it (`cargo test` / `go test ./...` / `npm test`), so the common case is zero-rubric.
  (confirm: how often does the inferred default match what the conductor would author?)
- **Apply-the-diff** — the manual `diff -ru "$PULLED" .` + surgical per-file copy is the
  worst ergonomic step; the CLI likely wants `--apply` (show diff, copy changed files into
  cwd) with foreign-WIP safety. (`pulled_to` is a temp staging dir outside cwd — see F3 —
  so there's nothing to `rm` in the repo; the friction is the copy-in, not cleanup.)
  (confirm: is review-then-apply always wanted, or sometimes apply-blind?)
- **Reap-on-exit** — `--ttl` + `prune` + `bookmark rm` is three gestures; the CLI likely
  wants the one-off case to self-reap losers after evidence is read. (CLI gesture, not skill.)
- **`--grader-file F` / out-of-workspace grading** (run #1 F2) — a forge-resistant reward
  must live outside the worker's editable snapshot. Hand-authoring + abs-pathing a hidden
  grader is the sharpest friction so far. The CLI should mount a grader at grade time
  (never snapshotted), so "objective reward" doesn't mean "and also build your own
  sandbox for it." (confirm: common case vs only self-referential tasks?)
- **cwd-independent worker handle** (run #1 F4) — `session list`/`rm` only see a project's
  workers from inside the project dir; `dispatch --json` could surface worker ids so a
  caller reaps without cd-ing. (confirm: does `--pillbox NAME` already suffice?)
- **k-sizing default** (run #1 F5) — resist reflexive `-k 3`; it's 3× cost for one answer
  unless the task has real failure variance. Not a flag, a conductor habit — but the CLI
  could hint (warn when all k tie) rather than silently waste forks.
