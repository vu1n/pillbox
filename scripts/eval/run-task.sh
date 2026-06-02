#!/usr/bin/env bash
# Run ONE (task, condition) through pillbox and grade it — the atomic unit of
# the "prove the loop" experiment (swarm-memory.md § "prove it before you build
# it"). Zero pillbox code: this CONSUMES `run` / `session send` / `session
# score` externally.
#
# Usage: run-task.sh <task-dir> <baseline|memory>
#   task-dir: holds prompt.txt + grade.sh (+ the starting workspace files)
#   condition: `memory` prepends scripts/eval/memory/playbook.md to the prompt
#
# Prints one TSV line: "<task>\t<condition>\t<pass|fail>". The verifiable score
# also lands as a `scored` §0 event on the session (then the session is removed).
#
# Env: PILLBOX (binary, default ./target/debug/pillbox), PILLBOX_RUNNER_IMAGE,
#      MAX_WAIT (seconds to wait for the turn, default 120).
set -euo pipefail

task_dir="${1:?usage: run-task.sh <task-dir> <baseline|memory>}"
condition="${2:?usage: run-task.sh <task-dir> <baseline|memory>}"
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
MAX_WAIT="${MAX_WAIT:-120}"
export PILLBOX_BACKEND=libkrun
task="$(basename "$task_dir")"

prompt="$(cat "$task_dir/prompt.txt")"
if [ "$condition" = "memory" ]; then
  prompt="$(cat "$here/memory/playbook.md")"$'\n\n'"$prompt"
fi

# A fresh copy of the starting workspace per run (the agent mutates a CoW clone
# of THIS dir, so each run must start from a pristine tree). The agent sees ONLY
# task/workspace/ — never task/grader/, so it can't read the test and hardcode.
ws="$(mktemp -d)"
trap 'rm -rf "$ws"' EXIT
cp -R "$task_dir/workspace/." "$ws"/

sid="$("$PILLBOX" run --agent opencode --workspace "$ws" 2>&1 | grep -oE '[0-9a-f]{12}' | head -1)"
[ -n "$sid" ] || { echo "$task	$condition	fail(no-session)"; exit 0; }

# The agent mutates the CoW clone recorded in the session handle, not $ws.
rec="$HOME/.pillbox/global/sessions/$sid.toml"
clone="$(python3 -c "import json,re;r=open('$rec').read();m=re.search(r'sandbox_id = (.+)',r);print(json.loads(eval(m.group(1)))['workspace'])")"
events="$(python3 -c "import json,re;r=open('$rec').read();m=re.search(r'sandbox_id = (.+)',r);print(json.loads(eval(m.group(1)))['creds']+'/.pillbox-opencode-events.sse')")"

"$PILLBOX" session send "$sid" "$prompt" >/dev/null 2>&1

# Wait for the turn to go idle (opencode emits session.idle on the /event
# stream the guest captures to $events), capped by MAX_WAIT.
for _ in $(seq 1 "$((MAX_WAIT / 2))"); do
  sleep 2
  grep -aq 'session.idle' "$events" 2>/dev/null && break
done

# Inject the hidden grader into the agent's edited clone, THEN grade — so the
# verifier ran against the agent's solution + an untampered test it never saw.
cp -R "$task_dir/grader/." "$clone"/
verdict="fail"
if "$PILLBOX" session score "$sid" --cmd "sh grade.sh" --workspace "$clone" 2>&1 | grep -q 'passed'; then
  verdict="pass"
fi
"$PILLBOX" session rm "$sid" >/dev/null 2>&1 || true
echo "$task	$condition	$verdict"
