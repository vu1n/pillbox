#!/usr/bin/env bash
# rubric-loop.sh — sample in-loop rubric verifier over pillbox's drive surface.
#
# The inference-time self-correction loop (cf. LangChain's RubricMiddleware),
# reimplemented harness-agnostically as an EXTERNAL orchestrator: it consumes
# `run` / `session send` / `session wait-idle` / `session score --rubric` — zero
# pillbox code. The loop:
#
#   drive the agent → score against the rubric → if criteria fail and we're under
#   --max-iter, inject the per-criterion feedback and re-drive → repeat.
#
# Terminal verdicts (the 5-state machine, mirroring RubricMiddleware):
#   satisfied        all criteria pass                          (exit 0)
#   max_iterations   still failing at the cap                   (exit 1)
#   rubric_failed    the rubric produced no criteria (malformed) (exit 1)
#   grader_error     the grader couldn't run / no session        (exit 1)
#
# This is deliberately NOT a pillbox verb: the loop is policy (how many tries,
# how to word feedback, when to give up) and lives with the orchestrator. pillbox
# supplies the primitives; the harness composes them.
#
# Usage: rubric-loop.sh <task-dir> [--max-iter N] [--json]
#   task-dir/
#     prompt.txt   the task instruction (no test leaked)
#     workspace/   the agent's starting tree, copied pristine (all it sees)
#     rubric.txt   host-side `NAME :: COMMAND` criteria — NEVER copied into the
#                  workspace, so the agent can't read the checks and hardcode.
#
# Env: PILLBOX (binary), MODEL (provider/modelID), MAX_WAIT (per-turn idle cap,
#      seconds), PILLBOX_RUNNER_IMAGE.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
MAX_WAIT="${MAX_WAIT:-120}"
export PILLBOX_BACKEND=libkrun
# shellcheck source=lib.sh
. "$here/lib.sh"  # pb_run_session / pb_workspace / pb_drive_and_wait

task="" max_iter=3 json=0
while [ $# -gt 0 ]; do case "$1" in
  --max-iter) max_iter="$2"; shift 2;;
  --json) json=1; shift;;
  -*) echo "unknown flag: $1" >&2; exit 2;;
  *) task="$1"; shift;;
esac; done
[ -n "$task" ] || { echo "usage: rubric-loop.sh <task-dir> [--max-iter N] [--json]" >&2; exit 2; }
case "$max_iter" in ''|*[!0-9]*|0) echo "--max-iter must be a positive integer" >&2; exit 2;; esac
rubric="$task/rubric.txt"
{ [ -f "$task/prompt.txt" ] && [ -d "$task/workspace" ] && [ -f "$rubric" ]; } || {
  echo "task dir must contain prompt.txt + workspace/ + rubric.txt" >&2; exit 2; }

# In --json mode the only stdout line is the final verdict object, so progress
# goes to stderr; in human mode it's all stdout.
note() { if [ "$json" = 1 ]; then echo "$@" >&2; else echo "$@"; fi; }

# emit_verdict <verdict> <iterations> <score> <criteria-json>
emit_verdict() {
  if [ "$json" = 1 ]; then
    python3 -c 'import json,sys
print(json.dumps({"verdict":sys.argv[1],"iterations":int(sys.argv[2]),
                  "score":float(sys.argv[3] or 0),"criteria":json.loads(sys.argv[4] or "[]")}))' \
      "$1" "$2" "${3:-0}" "${4:-[]}"
  else
    note "verdict: $1 — $2 iteration(s), score ${3:-0}"
  fi
  [ "$1" = satisfied ] && exit 0 || exit 1
}

# Start the session on a pristine copy of the workspace (the agent mutates a CoW
# clone; we keep the source tree clean for reproducibility).
ws="$(mktemp -d)"; trap 'rm -rf "$ws"' EXIT
cp -R "$task/workspace/." "$ws"/
# `|| true`: pb_run_session is a `pillbox | python3` pipeline; under `set -o
# pipefail` a failed launch makes the assignment nonzero, which `set -e` would
# abort on BEFORE the guard below — so tolerate it and let the guard emit the
# grader_error verdict (the loop's documented contract).
sid="$(pb_run_session "$ws")" || true
[ -n "$sid" ] || emit_verdict grader_error 0 0 '[]'
# From here, always tear the session down on exit.
trap '"$PILLBOX" session rm "$sid" >/dev/null 2>&1 || true; rm -rf "$ws"' EXIT

clone="$(pb_workspace "$sid")" || true
[ -n "$clone" ] || emit_verdict grader_error 0 0 '[]'

prompt="$(cat "$task/prompt.txt")"
last_sj="" score=0 iters=0

for i in $(seq 1 "$max_iter"); do
  iters="$i"
  note "── iteration $i/$max_iter ──"

  # Drive this turn (task prompt first; per-criterion feedback on later turns)
  # and block until the §0 idle signal, draining the trajectory meanwhile.
  pb_drive_and_wait "$sid" "$prompt"

  # Score against the rubric in a THROWAWAY copy of the agent's clone, so the
  # rubric's commands (and any __pycache__/artifacts they create) never touch the
  # tree the agent edits next turn — the checks stay invisible to the agent.
  scoredir="$(mktemp -d)"
  cp -R "$clone"/. "$scoredir"/ 2>/dev/null || true
  # `|| true`: a grader that couldn't run yields empty `$sj`, which the classifier
  # below maps to `grader_error` — don't let `set -e` abort the loop instead.
  sj="$("$PILLBOX" session score "$sid" --rubric "$rubric" --workspace "$scoredir" --json 2>/dev/null || true)"
  rm -rf "$scoredir"
  last_sj="$sj"

  # Classify the verdict from the structured §0 result (no stdout scrape).
  state="$(printf '%s' "$sj" | python3 -c 'import json,sys
try: d=json.load(sys.stdin)
except Exception: print("grader_error"); sys.exit()
c=d.get("criteria",[])
if not c: print("rubric_failed")
elif all(x.get("passed") for x in c): print("satisfied")
else: print("needs_revision")')"
  score="$(printf '%s' "$sj" | python3 -c 'import json,sys
try: print("%.3f"%json.load(sys.stdin).get("score",0))
except Exception: print("0")')"
  note "  score=${score} state=${state}"

  case "$state" in
    satisfied|rubric_failed|grader_error) break;;
    needs_revision)
      # Build the next prompt from the FAILED criteria — the targeted, per-
      # criterion gradient RubricMiddleware injects, not a bare "try again".
      prompt="$(printf '%s' "$sj" | python3 -c 'import json,sys
d=json.load(sys.stdin); c=d.get("criteria",[])
fails=[x for x in c if not x.get("passed")]
out=["Your solution does not yet pass all checks (%d/%d). Fix ONLY these failing"
     " criteria, then stop and wait:"%(len(c)-len(fails),len(c))]
for x in fails:
    fb=(x.get("feedback") or "").strip()
    out.append("\n\n- %s%s"%(x["name"], (":\n"+fb) if fb else ""))
sys.stdout.write("".join(out))')"
      ;;
    *) state=grader_error; break;;
  esac
done

# Map the loop exit to a terminal verdict + final criteria snapshot.
criteria="$(printf '%s' "$last_sj" | python3 -c 'import json,sys
try: print(json.dumps(json.load(sys.stdin).get("criteria",[])))
except Exception: print("[]")')"
case "${state:-}" in
  satisfied|rubric_failed|grader_error) emit_verdict "$state" "$iters" "$score" "$criteria";;
  *) emit_verdict max_iterations "$iters" "$score" "$criteria";;
esac
