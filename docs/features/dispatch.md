---
id: dispatch
paths:
  - "src/commands/dispatch.rs"
  - "src/commands/session/grader.rs"
  - "src/commands/eval.rs"
  - "src/commands/critic.rs"
  - "src/workspace/protect.rs"
  - "rubrics/**"
  - "scripts/smoke/dispatch*.sh"
entry:
  - "src/commands/dispatch.rs"
  - "src/commands/session/grader.rs"
  - "src/workspace/protect.rs"
tests:
  - "src/commands/dispatch.rs:a_worker_editing_a_protected_test_does_not_change_its_grade"
  - "src/commands/dispatch.rs:worker_build_and_test_code_never_runs_on_the_host"
  - "src/commands/dispatch.rs:a_fork_starts_exactly_what_the_old_cli_argv_asked_for"
  - "src/commands/eval.rs:a_cell_starts_exactly_what_the_old_cli_argv_asked_for"
  - "scripts/smoke/dispatch.sh"
  - "scripts/smoke/dispatch-segments.sh"
---

# Verified worker selection and segment chains

An orchestrator forks workers from a bookmark, grades their work with a command or rubric, retries bounded failures and selects a winner. Segments chain focused prompts inside each worker’s session. See [dispatch](../dispatch.md), [eval](../eval.md) and the repository dispatch skill.

## Sub-features

- Best-of-k diversity and cumulative in-session segment gates.
- Protected grading workspace in a separate grader microVM.
- Run-level reward, worker evidence, winner selection and workspace pull.

## How to get to it (user POV)

Create a base snapshot/bookmark and a verifier, then use `pillbox dispatch --from-bookmark base -k 1 --cmd "your verifier" --agent opencode --json -- "task"`. The response contains worker outcomes, winner and `pulled_to`. `--segments SPEC` adds an ordered TOML segment chain.

## Driving it with the repository harness

`cargo test --no-default-features commands::dispatch` runs offline parsing, selection and fake-grader tests, including protected tests and absence of host execution. The live driver (`SessionDriver`) calls the session API in-process; its workspace lookup, grading and pull go through the `SessionOps` seam, which the grading tests replace with a recorder that refuses non-`in_sandbox` grades. On a signed libkrun build with configured image/auth, `scripts/smoke/dispatch.sh` and `scripts/smoke/dispatch-segments.sh` drive live worker selection and segment behavior; these consume model turns.

## Gotchas

- A segment gate steers progression; it is not the authoritative run-level reward.
- Worker-edited tests are restored/protected for grading. Grader unavailability is an error, not a host fallback.
- Inspect `pulled_to`, not only winner/exit status; result recovery can fail separately.
- Dispatch never re-executes `pillbox` or parses CLI `--json`; a fork that comes back as anything but a detached/server session (or a structured run that exited 0) errors that worker (`Started::into_live_session`).
- Forks run `run::start` inside the long-lived dispatch process, so each detached `__krun-vmm`/`__session-tailer` child is a `sandbox::DetachedChild` whose `Drop` hands it to a waiter thread; a bare dropped `Child` that exits would stay a zombie that the tailer's signal-0 liveness probe reads as alive, and `wait_idle` would never start a replacement tailer.
- A general Ghost planner/conductor DAG remains design history, not a feature implied by the implemented segment chain.
