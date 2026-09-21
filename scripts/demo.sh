#!/usr/bin/env bash
# pillbox demo: the full lifecycle of one session in a few minutes.
#
#   1. Create a throwaway workspace and a project pillbox for it.
#   2. Start a detached server-mode agent in a libkrun microVM.
#   3. Drive one turn (`session send`) and wait for it to go idle.
#   4. Read the session back: diagnose (status + conditions), the §0 log.
#   5. Pull the agent's workspace out and show the original was never touched.
#   6. Grade the result with an executable check (`session score`).
#
# Every command is printed the way you would type it, then run. Nothing here is
# a mock: it boots a real VM, drives a real model, and reads the real §0 log.
#
# Environment:
#   PILLBOX        path to the pillbox binary          (default: pillbox on PATH)
#   MODEL          provider/model for opencode          (default: zai-coding-plan/glm-4.5-air)
#   AGENT          server-mode agent to drive           (default: opencode)
#   MAXWAIT        idle wait cap in seconds             (default: 300)
#   KEEP           set to leave the session running at the end (default: torn down)
#   NO_COLOR       set to disable colored output
#
# Prereqs: `pillbox doctor` green, the agent authenticated (`pillbox auth login
# --agent $AGENT` or a model key it can use), the runner image present.

set -euo pipefail

PB="${PILLBOX:-pillbox}"
MODEL="${MODEL:-zai-coding-plan/glm-4.5-air}"
AGENT="${AGENT:-opencode}"
MAXWAIT="${MAXWAIT:-300}"

# pillbox runs the binary at $PILLBOX so the commands below read the way you'd type them.
pillbox() { "${PB}" "$@"; }

# ---------------------------------------------------------------------------
# Presentation helpers
# ---------------------------------------------------------------------------

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
  BOLD=$'\e[1m'; DIM=$'\e[2m'; CYAN=$'\e[36m'; GREEN=$'\e[32m'; YELLOW=$'\e[33m'; RESET=$'\e[0m'
else
  BOLD=""; DIM=""; CYAN=""; GREEN=""; YELLOW=""; RESET=""
fi

STEP=0
step() {
  STEP=$((STEP + 1))
  printf '\n%s%s━━ %d. %s%s\n\n' "${BOLD}" "${CYAN}" "${STEP}" "$*" "${RESET}"
}

# run prints the command the way you would type it, then runs it.
run() {
  printf '%s$ %s%s\n' "${DIM}" "$*" "${RESET}"
  "$@"
}

ok()   { printf '%s✔ %s%s\n' "${GREEN}" "$*" "${RESET}"; }
note() { printf '%s%s%s\n' "${YELLOW}" "$*" "${RESET}"; }
die()  { printf '%s✗ %s%s\n' "${YELLOW}" "$*" "${RESET}" >&2; exit 1; }

# condition TYPE reads one condition's status off `session diagnose --json`.
condition() {
  pillbox session diagnose "${SID}" --json 2>/dev/null \
    | jq -r --arg t "$1" '.session.conditions[] | select(.type == $t) | .status'
}

# wait_for TYPE [TIMEOUT] polls a condition until it is True, printing a dot per
# poll and the elapsed time when it gets there.
wait_for() {
  local want="$1" timeout="${2:-${MAXWAIT}}"
  local start now
  start=$(date +%s)
  printf '%swaiting for %s=True%s ' "${DIM}" "${want}" "${RESET}"
  while :; do
    if [[ "$(condition "${want}")" == "True" ]]; then
      printf ' %s%ds%s\n' "${GREEN}" "$(( $(date +%s) - start ))" "${RESET}"
      return 0
    fi
    now=$(date +%s)
    if (( now - start > timeout )); then
      printf '\n'
      note "Gave up after ${timeout}s. Last seen:"
      pillbox session diagnose "${SID}" || true
      return 1
    fi
    printf '.'
    sleep 2
  done
}

# ---------------------------------------------------------------------------
# Demo
# ---------------------------------------------------------------------------

command -v jq >/dev/null || die "jq is required"
command -v "${PB}" >/dev/null || die "no pillbox binary at ${PB} (set PILLBOX=…)"

WS="$(mktemp -d)"
PULLED="$(mktemp -d)"
SID=""
cleanup() {
  if [[ -n "${SID}" && -z "${KEEP:-}" ]]; then
    pillbox session rm "${SID}" >/dev/null 2>&1 || true
  fi
  rm -rf "${WS}" "${PULLED}"
}
trap cleanup EXIT

printf '\n%s🚀 pillbox demo%s  %sagent=%s model=%s%s\n' \
  "${BOLD}" "${RESET}" "${DIM}" "${AGENT}" "${MODEL}" "${RESET}"

step "Preflight"
run pillbox doctor

step "Create a workspace and a project pillbox for it"
# A mechanical, unambiguous task so the grader is a real check, not a vibe.
printf 'def f(a, b):\n    return a - b\n' >"${WS}/bug.py"
printf 'from bug import f\nassert f(2, 2) == 4, f(2, 2)\nprint("ok")\n' >"${WS}/check.py"
printf '%s' "${DIM}"; sed 's/^/    /' "${WS}/bug.py"; printf '%s\n' "${RESET}"
( cd "${WS}" && run pillbox new --name demo --agent "${AGENT}" )

step "Start an agent in a microVM"
note "The VM boots, the workspace is forked from a snapshot (your dir is never touched), and a server-mode agent comes up reparented — run returns, the session stays."
printf '%s$ pillbox run --agent %s --model %s --json | jq -r .session.id%s\n' "${DIM}" "${AGENT}" "${MODEL}" "${RESET}"
SID="$(cd "${WS}" && pillbox run --agent "${AGENT}" --model "${MODEL}" --json | jq -r '.session.id')"
[[ -n "${SID}" ]] || die "run did not start a session"
ok "session ${SID}"
echo
run pillbox session list

step "Drive one turn and wait for the agent to go idle"
run pillbox session send "${SID}" $'In bug.py, change `return a - b` to `return a + b`. Edit the file.\n'
run pillbox session wait-idle "${SID}" --timeout "${MAXWAIT}"
ok "turn complete"

step "Read the session back"
note "diagnose folds the §0 log into a status plus typed conditions; AwaitingInput is the one a driver waits on."
run pillbox session diagnose "${SID}"
echo
note "The log itself — one JSON event per line, replayable from any seq:"
printf '%s$ pillbox session log %s --type tool_call | head -3%s\n' "${DIM}" "${SID}" "${RESET}"
pillbox session log "${SID}" --type tool_call | head -3

step "Pull the agent's workspace out"
run pillbox session pull "${SID}" --to "${PULLED}"
grep -q 'return a + b' "${PULLED}/bug.py" && ok "the pulled workspace carries the edit" \
  || note "the pulled workspace does not carry the edit — read the diagnose output above"
grep -q 'return a - b' "${WS}/bug.py" && ok "the original workspace was never touched (fork-from-store)"

step "Grade the result with an executable check"
note "The verifier's exit code is the reward and a scored event lands in the §0 log. This is not the agent's self-report."
run pillbox session score "${SID}" --workspace "${PULLED}" --cmd 'python3 check.py' --json

printf '\n%s🎉 Demo complete.%s\n\n' "${BOLD}" "${RESET}"
if [[ -n "${KEEP:-}" ]]; then
  printf 'Session %s is still running. Pick it up, read it, or clean up with:\n\n' "${SID}"
  printf '    pillbox session attach %s\n' "${SID}"
  printf '    pillbox session watch %s\n' "${SID}"
  printf '    pillbox session rm %s\n\n' "${SID}"
else
  printf 'The session is torn down on exit. Set KEEP=1 to leave it running and:\n\n'
  printf '    pillbox session attach <id>     # take the terminal\n'
  printf '    pillbox session watch <id>      # read the log as it grows\n'
  printf '    pillbox session send <id> "…"   # drive the next turn\n\n'
fi
