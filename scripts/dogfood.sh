#!/usr/bin/env bash
# dogfood.sh — use pillbox + kypp on a REAL repo, with memory that COMPOUNDS across runs.
#
# The "use it for real" on-ramp. Memory lives in the real kypp store (~/.kypp), scoped to
# KYPP_PROJECT = the repo's basename, so each repo grows its own DNA and it persists across
# runs. The agent edits a FORK (fork-from-store — libkrun CoW); YOUR repo is never touched,
# so you review the diff and apply what's good.
#
#   scripts/dogfood.sh run     <repo> "<task>"          do a task (memory briefed + captured)
#   scripts/dogfood.sh correct <repo> "<subject>" "<x>" teach it (a correction that sticks next run)
#   scripts/dogfood.sh mem     <repo>                    show the repo's accrued memory
#
# The compounding loop: `run` briefs the repo's memory into the prompt, drives the agent,
# then distils the trajectory back into that repo's memory (failures → pitfalls). `correct`
# is the human channel — the strongest signal, surfaced first in every future brief. Set
# KYPP_DISTILL_MODEL for richer LLM-distilled lessons (else: failure-mining only).
#
# Env: PILLBOX (binary), PILLBOX_BACKEND (=libkrun), PILLBOX_RUNNER_IMAGE (=pillbox-runner:dev),
#      MODEL (=zai-coding-plan/glm-4.5-air), MAXWAIT (idle cap, =600s).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PB="${PILLBOX:-$ROOT/target/debug/pillbox}"
export PILLBOX_BACKEND="${PILLBOX_BACKEND:-libkrun}"
export PILLBOX_RUNNER_IMAGE="${PILLBOX_RUNNER_IMAGE:-pillbox-runner:dev}"
MODEL="${MODEL:-zai-coding-plan/glm-4.5-air}"
MAXWAIT="${MAXWAIT:-600}"

die() { echo "dogfood: $1" >&2; exit 1; }
proj() { basename "$(cd "$1" 2>/dev/null && pwd || echo "$1")"; }

usage() {
  sed -n '3,18p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

need_kypp() { command -v kypp >/dev/null || die "kypp not on PATH — install: uv tool install ~/code/kypp"; }

need_bin() {
  [ -x "$PB" ] || die "no pillbox binary at $PB — run scripts/lk-build.sh"
  # grep -c in $(), NOT `grep -q` in a pipe: under `pipefail`, grep -q short-circuits → the
  # producer gets SIGPIPE → the pipeline falsely reports non-zero even on a match.
  if [ "$PILLBOX_BACKEND" = libkrun ]; then
    [ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] || die "binary lacks libkrun — run scripts/lk-build.sh"
    [ "$(codesign -d --entitlements :- "$PB" 2>/dev/null | grep -c hypervisor)" -ge 1 ] || die "binary not codesigned — run scripts/lk-build.sh"
  fi
}

cmd="${1:-}"; [ $# -gt 0 ] && shift

case "$cmd" in
  correct)
    need_kypp
    repo="${1:?usage: dogfood.sh correct <repo> \"<subject>\" \"<content>\"}"
    KYPP_PROJECT="$(proj "$repo")" kypp correct "${2:?subject}" "${3:?content}"
    ;;

  mem)
    need_kypp
    repo="${1:?usage: dogfood.sh mem <repo>}"; P="$(proj "$repo")"
    echo "▶ accrued memory for [$P]:"
    KYPP_PROJECT="$P" kypp briefing --project "$P" --candidates
    ;;

  run)
    need_kypp; need_bin
    repo="$(cd "${1:?usage: dogfood.sh run <repo> \"<task>\"}" && pwd)" || die "no such repo"
    task="${2:?task}"; P="$(proj "$repo")"
    export KYPP_PROJECT="$P"
    echo "▶ dogfood [$P]: $task"

    brief="$(kypp briefing --project "$P" 2>/dev/null)"
    if [ -n "$brief" ] && ! printf '%s' "$brief" | grep -q "no accepted memory"; then
      echo "── memory briefed ──"; printf '%s\n' "$brief" | sed 's/^/  /'
      prompt=$'## Project memory (kypp)\n'"$brief"$'\n\n## Task\n'"$task"
    else
      echo "── (no memory yet for [$P] — this run starts building it) ──"; prompt="$task"
    fi

    sid="$("$PB" run --agent opencode --json --workspace "$repo" --model "$MODEL" 2>/tmp/dogfood.err \
           | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["session"]["id"])
except Exception: pass')"
    [ -n "$sid" ] || { tail -4 /tmp/dogfood.err 2>/dev/null; die "run did not start (model reachable? bring-up hung?)"; }
    echo "  session $sid — driving (≤${MAXWAIT}s)…"
    "$PB" session send "$sid" "$prompt" >/dev/null 2>&1
    "$PB" session wait-idle "$sid" --timeout "$MAXWAIT" >/dev/null 2>&1 || echo "  (turn ran past ${MAXWAIT}s — using what landed)"

    # Compounding: distil this trajectory into THIS repo's memory. wait-idle already drained
    # the §0 log, so capture reads it directly. --project pins it to the repo (not the log's
    # pillbox scope). Failures → pitfalls; a distill model also generalizes success procedures.
    if KYPP_PROJECT="$P" kypp capture --session "$sid" --distill --project "$P" >/dev/null 2>&1; then
      echo "  ✓ trajectory distilled → memory[$P]"
    else
      echo "  · capture skipped"
    fi

    out="$(mktemp -d)"; "$PB" session pull "$sid" --to "$out" >/dev/null 2>&1
    echo "── changes (your repo is UNTOUCHED — review the fork, apply what's good) ──"
    ( cd "$out" && find . -type f -not -path '*/.git/*' -not -path '*/node_modules/*' -not -path '*/target/*' ) \
      | while IFS= read -r f; do
          rel="${f#./}"
          if [ ! -e "$repo/$rel" ]; then echo "  + $rel"
          elif ! cmp -s "$out/$f" "$repo/$rel"; then echo "  ~ $rel"; fi
        done
    echo "  fork: $out"
    echo "  apply: cp -R \"$out/.\" \"$repo/\"   (after review)   ·   teach: dogfood.sh correct $repo \"<subject>\" \"<lesson>\""
    "$PB" session rm "$sid" >/dev/null 2>&1
    ;;

  ""|-h|--help|help) usage 0 ;;
  *) echo "dogfood: unknown command ${cmd@Q}" >&2; usage 2 ;;
esac
