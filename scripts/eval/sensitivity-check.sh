#!/usr/bin/env bash
# The gate-before-the-gate (docs/optimization-eval-family.md §5): prove the eval
# rig can DETECT a known, planted lift at a feasible trial count before spending
# a real GEPA-vs-baseline-vs-ACE bakeoff. Three prior runs (gate, small-worker,
# kypp) all died at variance >> effect; running a fourth bakeoff without this
# check repeats the mistake.
#
# For every task, run TWO arms — baseline (prompt only) and oracle (prompt + a
# small, TRUE hint that strictly helps, the planted lift a perfect router/memory
# would surface) — TRIALS times each at TEMPERATURE=0 (greedy, the variance
# knob), capturing each trial's rubric score + $cost. Emit one JSONL record per
# trial and pipe them to paired-stats.py, which reports σ̂ + the paired-lift CI
# and the verdict: is the rig sensitive enough (σ̂ low AND the planted lift's CI
# excludes 0)?
#
# Usage: sensitivity-check.sh <oracle-profile> <task-ref...>
#   oracle-profile : a file prepended to arm-B's prompt — the planted hint. Must
#                    be TRUE for every task (e.g. all synthetic bugs in the same
#                    file → "the bug is in solution.py"), else it's noise not lift.
#   task-ref       : a task dir (prompt.txt+workspace/+grader/) or a frozen
#                    bookmark <set>/<split>/<id> in the evals pillbox.
# Env: PILLBOX, MODEL, TRIALS (default 3), MAX_WAIT, PRICE_*_PER_M (cost-summer),
#      OUT (JSONL records path; default a tempfile, printed at the end).
#
# Greedy decoding is forced here (TEMPERATURE=0). If opencode silently ignores
# per-prompt temperature, σ̂ stays high and the verdict is "not sensitive" — so
# this check also verifies its own temp-0 prerequisite.
set -euo pipefail

u="usage: sensitivity-check.sh <oracle-profile> <task-ref...>"
oracle="${1:?$u}"; shift
[ -f "$oracle" ] || { echo "oracle profile not found: $oracle" >&2; exit 2; }
[ "$#" -ge 1 ] || { echo "$u" >&2; exit 2; }

here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
MAX_WAIT="${MAX_WAIT:-120}"
TRIALS="${TRIALS:-3}"
EVALS_PILLBOX="${EVALS_PILLBOX:-evals}"
export PILLBOX_BACKEND=libkrun
export TEMPERATURE="${TEMPERATURE:-0}" # greedy by default — the whole point
# shellcheck source=lib.sh
. "$here/lib.sh"

out="${OUT:-$(mktemp)}"
: >"$out"

# Resolve a task ref to a local dir (a dir is used in place; a bookmark is pulled
# back so every run starts from the byte-identical frozen tree). Echoes the dir.
resolve_task() {
  local ref="$1"
  if [ -d "$ref" ]; then printf '%s' "$ref"; return; fi
  local d; d="$(mktemp -d)"
  if ! ( cd "$d" && "$PILLBOX" --pillbox "$EVALS_PILLBOX" pull --bookmark "$ref" ) >/dev/null 2>&1; then
    rm -rf "$d"; return 1
  fi
  printf '%s' "$d"
}

# One (task, condition, trial): run greedy, drive, grade in the agent's clone
# against the untampered hidden grader, read score + cost from the §0 surfaces,
# tear down. Appends one JSONL record. `profile` is empty for baseline.
run_cell() {
  local task_dir="$1" task="$2" cond="$3" trial="$4" profile="$5"
  local prompt; prompt="$(cat "$task_dir/prompt.txt")"
  [ -n "$profile" ] && prompt="$(cat "$profile")"$'\n\n'"$prompt"

  local ws; ws="$(mktemp -d)"
  cp -R "$task_dir/workspace/." "$ws"/
  local sid; sid="$(pb_run_session "$ws")"
  if [ -z "$sid" ]; then rm -rf "$ws"; return; fi
  local clone; clone="$(pb_workspace "$sid")"
  if [ -z "$clone" ]; then "$PILLBOX" session rm "$sid" >/dev/null 2>&1; rm -rf "$ws"; return; fi

  pb_drive_and_wait "$sid" "$prompt"
  cp -R "$task_dir/grader/." "$clone"/
  local grade=(--cmd "sh grade.sh")
  [ -f "$task_dir/grader/rubric.txt" ] && grade=(--rubric "$task_dir/grader/rubric.txt")
  local score_json; score_json="$("$PILLBOX" session score "$sid" "${grade[@]}" --workspace "$clone" --json 2>/dev/null || true)"
  local score; score="$(printf '%s' "$score_json" | pb_score_value)"
  # Read cost BEFORE rm (the §0 log is gone after teardown).
  local usage; usage="$(pb_usage "$sid")"
  "$PILLBOX" session rm "$sid" >/dev/null 2>&1 || true
  rm -rf "$ws"

  python3 -c '
import json,sys
task,cond,trial,score,usage=sys.argv[1:6]
cost=0.0
try: cost=json.loads(usage).get("costUsd",0.0)
except Exception: pass
print(json.dumps({"task":task,"cond":cond,"trial":int(trial),"score":float(score),"cost":cost}))
' "$task" "$cond" "$trial" "${score:-0}" "${usage:-{}}" >>"$out"
}

for ref in "$@"; do
  task="$(basename "$ref")"
  if ! task_dir="$(resolve_task "$ref")"; then
    echo "skip (no frozen task): $ref" >&2; continue
  fi
  for t in $(seq 1 "$TRIALS"); do
    run_cell "$task_dir" "$task" baseline "$t" ""        # arm A
    run_cell "$task_dir" "$task" oracle   "$t" "$oracle" # arm B (planted lift)
  done
  [ -d "$ref" ] || rm -rf "$task_dir"
done

echo "=== records: $out ===" >&2
echo "=== verdict (paired-stats) ===" >&2
python3 "$here/paired-stats.py" --baseline baseline --treatment oracle "$out"
