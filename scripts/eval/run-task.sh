#!/usr/bin/env bash
# Run ONE (task, condition) through pillbox and grade it — the atomic unit of
# the "prove the loop" experiment (swarm-memory.md § "prove it before you build
# it"). Zero pillbox code: this CONSUMES `run` / `session send` / `session
# score` externally.
#
# Usage: run-task.sh <task-dir|frozen-bookmark> <baseline|memory>
#   task source: a dir (prompt.txt + workspace/ + grader/), OR a frozen-snapshot
#                bookmark <set>/<split>/<id> in the evals pillbox (freeze-task.sh)
#                — pulled back so every run starts from the identical tree.
#   condition: `memory` prepends scripts/eval/memory/playbook.md to the prompt
#
# Prints one TSV line: "<task>\t<condition>\t<pass|fail>". The verifiable score
# also lands as a `scored` §0 event on the session (then the session is removed).
#
# Env: PILLBOX (binary, default ./target/debug/pillbox), PILLBOX_RUNNER_IMAGE,
#      MAX_WAIT (seconds to wait for the turn, default 120).
set -euo pipefail

u="usage: run-task.sh <task-dir|frozen-bookmark> <baseline|memory>"
task_ref="${1:?$u}"
condition="${2:?$u}"
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
MAX_WAIT="${MAX_WAIT:-120}"
EVALS_PILLBOX="${EVALS_PILLBOX:-evals}"
export PILLBOX_BACKEND=libkrun
# shellcheck source=lib.sh
. "$here/lib.sh"  # pb_run_session / pb_workspace / pb_drive_and_wait

# Resolve the task source. A directory is used in place (legacy / un-frozen).
# Otherwise it's a frozen-snapshot bookmark (<set>/<split>/<id>) in the evals
# pillbox: pull it back so every run starts from the IDENTICAL immutable tree —
# the dogfooded freeze (`pillbox push --bookmark` froze it). $frozen is cleaned
# by the trap below alongside the agent's working copy.
frozen=""
if [ -d "$task_ref" ]; then
  task_dir="$task_ref"
else
  frozen="$(mktemp -d)"
  if ! ( cd "$frozen" && "$PILLBOX" --pillbox "$EVALS_PILLBOX" pull --bookmark "$task_ref" ) >/dev/null 2>&1; then
    rm -rf "$frozen"
    echo "$(basename "$task_ref")	$condition	fail(no-frozen-task:$task_ref)"
    exit 0
  fi
  task_dir="$frozen"
fi
task="$(basename "$task_ref")"

# `condition` selects the injected profile (prepended to the prompt):
#   baseline           → nothing
#   memory             → memory/playbook.md (the curated bullets)
#   <path to a file>   → that file (the meta-harness injects candidate profiles)
prompt="$(cat "$task_dir/prompt.txt")"
profile=""
case "$condition" in
  baseline) : ;;
  memory)   profile="$here/memory/playbook.md" ;;
  *)        profile="$condition" ;;
esac
if [ -n "$profile" ] && [ -f "$profile" ]; then
  prompt="$(cat "$profile")"$'\n\n'"$prompt"
fi

# A fresh copy of the starting workspace per run (the agent mutates a CoW clone
# of THIS dir, so each run must start from a pristine tree). The agent sees ONLY
# task/workspace/ — never task/grader/, so it can't read the test and hardcode.
ws="$(mktemp -d)"
trap 'rm -rf "$ws" "$frozen"' EXIT
cp -R "$task_dir/workspace/." "$ws"/

# MODEL (provider/modelID) overrides opencode's default — set it to a capable
# model so the baseline lands in a measurable band (GLM-4.5-air floors hard sets,
# leaving no headroom to detect a memory delta).
sid="$(pb_run_session "$ws")"
[ -n "$sid" ] || { echo "$task	$condition	fail(no-session)"; exit 0; }

# The agent edits its result-workspace (a CoW clone), not $ws.
clone="$(pb_workspace "$sid")"
[ -n "$clone" ] || { echo "$task	$condition	fail(no-workspace)"; "$PILLBOX" session rm "$sid" >/dev/null 2>&1; exit 0; }

# Drive the turn + block until idle, draining the §0 trajectory into the log
# meanwhile (so the failure report reflects on HOW the agent worked). A timeout
# is treated as a fail — we still grade whatever landed.
pb_drive_and_wait "$sid" "$prompt"

# Inject the hidden grader into the agent's edited clone, THEN grade — so the
# verifier ran against the agent's solution + an untampered test it never saw.
# Read the verdict from the JSON surface (`passed`/`feedback`) — no stdout-scrape,
# no reach into the §0 log for the `scored` event.
cp -R "$task_dir/grader/." "$clone"/
# `|| true` + the readers' `except → fail/empty`: a grader that couldn't run
# (empty/malformed JSON) is a non-pass, not a harness crash — the batch must
# score the next task, not abort under `set -e`. The verdict schema reads live
# in lib.sh (pb_passed/pb_feedback), single-sourced across the orchestrators.
# Prefer the per-criterion rubric (graded fraction + which tests failed) when the
# task ships one; else the binary grade.sh. The rubric file is host-side
# (`--rubric`); its criteria run in the clone against the injected test module.
if [ -f "$task_dir/grader/rubric.txt" ]; then
  grade=(--rubric "$task_dir/grader/rubric.txt")
else
  grade=(--cmd "sh grade.sh")
fi
score_json="$("$PILLBOX" session score "$sid" "${grade[@]}" --workspace "$clone" --json 2>/dev/null || true)"
verdict="$(printf '%s' "$score_json" | pb_passed)"
feedback="$(printf '%s' "$score_json" | pb_feedback)"
score_val="$(printf '%s' "$score_json" | pb_score_value)"

# FAILDIR: on a fail, capture a failure report (task + what the agent produced +
# why it failed) — the input the meta-harness's `propose` step reflects on.
if [ "$verdict" = fail ] && [ -n "${FAILDIR:-}" ]; then
  mkdir -p "$FAILDIR"
  {
    echo "## TASK: $task"; echo; cat "$task_dir/prompt.txt"; echo
    echo "## THE AGENT PRODUCED:"
    for f in "$task_dir"/workspace/*; do
      b="$(basename "$f")"; echo "--- $b ---"; cat "$clone/$b" 2>/dev/null; echo
    done
    # Trajectory (tool calls, in order) via `session log --type tool_call` — the
    # GEPA-style textual gradient `propose` reflects on. Read through the §0 CLI
    # surface, NOT by opening the on-disk log path. Grader feedback comes from the
    # `score --json` verdict above, not a second pass over the log.
    "$PILLBOX" session log "$sid" --type tool_call 2>/dev/null | python3 -c '
import json, sys
order, status = [], {}
for line in sys.stdin:
    try:
        p = json.loads(line).get("payload", {})
    except Exception:
        continue
    cid = p.get("toolCallId") or p.get("name") or str(len(order))
    if cid not in status:
        order.append(cid)
    status[cid] = (p.get("name", "?"), p.get("status", ""))
print("## AGENT TRAJECTORY (tools, in order):")
if order:
    for cid in order:
        name, st = status[cid]
        print(f"- {name} [{st}]")
else:
    print("(no tool calls captured)")
'
    echo
    echo "## GRADER FEEDBACK (why it failed):"
    # tail -c, not ${var: -N} — macOS bash 3.2 has no negative substring offset.
    printf '%s' "$feedback" | tail -c 2000
    echo
  } > "$FAILDIR/$task.md" 2>/dev/null
fi

"$PILLBOX" session rm "$sid" >/dev/null 2>&1 || true
# TSV: task, condition, pass/fail (all criteria), fractional score [0,1]. The
# gate averages the score column — the low-noise metric a rubric buys over binary.
echo "$task	$condition	$verdict	${score_val:-0}"
