# Rubric library

Reusable, execution-grounded grading templates for `pillbox dispatch` and
`pillbox session score`. A rubric is a plain checklist of named criteria, each a
shell command run against the graded workspace.

See `.claude/skills/dispatch/SKILL.md` for when/how to dispatch, and
`docs/dispatch.md` for the contract.

## Format

One criterion per line:

```
NAME :: COMMAND
```

- `NAME` — a short label for the criterion (shown in per-criterion verdicts).
- `COMMAND` — a shell command run **in the graded workspace**. **Exit 0 = pass**,
  any non-zero = fail; the command's combined output becomes the criterion feedback.
- Lines that are blank or start with `#` are ignored (use `#` for comments/headers).
- The separator is the literal ` :: ` (space-colon-colon-space). Every non-comment
  line **must** contain it.

The score is the **passed fraction** (e.g. 3 of 4 criteria → 0.75); all-pass → 1.0.

This is exactly the format `pillbox session score --rubric` parses, so a rubric here
is portable across `score`, `dispatch --rubric`, and a segment's `gate_rubric`.

## Two roles: reward vs gate

The same rubric file serves two purposes — keep them distinct:

- **Reward** (`dispatch --rubric FILE`, `session score --rubric FILE`) — the
  **authoritative selector**. This decides the dispatch winner and is the
  forge-resistant, non-self-reported verdict. A full template here is meant as a
  reward.
- **Gate** (`gate_rubric` in a `--segments` spec) — only **steers progression**
  within a segment chain and feeds distilled retry feedback; a failed gate advances
  the chain, it does not select the winner. A gate is usually a **subset** scoped to
  one segment (e.g. just the `pytest -k <segment>` line), not the whole reward rubric.

## Goodhart reminder

The reward is the selector; never let a gate stand in for it, and never let an
agent's self-report ("done") count as a pass. Grades come from running commands, not
from the agent's word. Write criteria that test the *outcome*, not the *claim* — and
scope each command tightly so it fails for the right reason.

## Templates

| File | Grades |
|---|---|
| `rust-change.rubric` | A Rust change: fmt clean, clippy clean, build, tests green. |
| `test-pass.rubric` | A test suite is green (language-agnostic; edit to your runner). |
| `doc-change.rubric` | A doc change: file present, link/format invariants hold. |
| `repro-script.rubric` | A repro script exists and exits 0. |

These are starting points — copy and tighten the commands to the actual task. Every
command must be runnable in the graded workspace with no extra setup (gates are
self-contained).
