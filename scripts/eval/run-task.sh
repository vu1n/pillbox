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
sid="$("$PILLBOX" run --agent opencode --workspace "$ws" ${MODEL:+--model "$MODEL"} 2>&1 | grep -oE '[0-9a-f]{12}' | head -1)"
[ -n "$sid" ] || { echo "$task	$condition	fail(no-session)"; exit 0; }

# The agent edits its result-workspace (a CoW clone), not $ws. Read its path
# from the JSON surface — not by parsing pillbox's internal session record.
clone="$("$PILLBOX" session info "$sid" --json 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)["session"].get("workspace",""))')"
[ -n "$clone" ] || { echo "$task	$condition	fail(no-workspace)"; "$PILLBOX" session rm "$sid" >/dev/null 2>&1; exit 0; }

"$PILLBOX" session send "$sid" "$prompt" >/dev/null 2>&1

# Block until the turn goes idle (the §0 NeedsInput signal) — the drive-surface
# primitive, replacing the old grep-the-capture-file poll. It also drains the
# full §0 trajectory into the durable log while waiting, so the failure report
# reflects on HOW the agent worked (no separate `session ingest` needed). A
# timeout is treated as a fail (we still grade whatever landed).
"$PILLBOX" session wait-idle "$sid" --timeout "$MAX_WAIT" >/dev/null 2>&1 || true

# Inject the hidden grader into the agent's edited clone, THEN grade — so the
# verifier ran against the agent's solution + an untampered test it never saw.
cp -R "$task_dir/grader/." "$clone"/
verdict="fail"
if "$PILLBOX" session score "$sid" --cmd "sh grade.sh" --workspace "$clone" 2>&1 | grep -q 'passed'; then
  verdict="pass"
fi

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
    # Trajectory (from the ingested §0 log) + grader feedback — both from the
    # real session log, the GEPA-style textual gradient `propose` reflects on.
    python3 - "$sid" <<'PY'
import json, os, sys
log = os.path.expanduser(f"~/.pillbox/global/sessions/{sys.argv[1]}/log.jsonl")
order, status, fb = [], {}, ""
for line in open(log):
    try:
        p = json.loads(line).get("payload", {})
    except Exception:
        continue
    t = p.get("type")
    if t == "tool_call":
        cid = p.get("toolCallId") or p.get("name") or str(len(order))
        if cid not in status:
            order.append(cid)
        status[cid] = (p.get("name", "?"), p.get("status", ""))
    elif t == "scored":
        fb = p.get("feedback", "")
print("## AGENT TRAJECTORY (tools, in order):")
if order:
    for cid in order:
        name, st = status[cid]
        print(f"- {name} [{st}]")
else:
    print("(no tool calls captured)")
print()
print("## GRADER FEEDBACK (why it failed):")
print(fb[-2000:])
PY
  } > "$FAILDIR/$task.md" 2>/dev/null
fi

"$PILLBOX" session rm "$sid" >/dev/null 2>&1 || true
echo "$task	$condition	$verdict"
