#!/usr/bin/env bash
# Live e2e smoke for `pillbox dispatch` (GHOST-004): fork k workers from a
# bookmark → drive each (send the segment prompt) → score → select → pull. The
# first real exercise of the dispatch CliDriver on booted VMs — unit tests cover
# the selection/retry POLICY, this covers the fork/drive/score/pull WIRING and
# the agent turn-semantics they can't reach.
#
#   Usage: scripts/smoke/dispatch.sh [runner-image] [model]
# Env: SMOKE_MODEL (model), PILLBOX_DISPATCH_TURN_TIMEOUT (per-turn idle wait).
# Prereqs: codesigned libkrun binary (scripts/lk-build.sh), opencode authed
# (`pillbox auth login --agent opencode`), the runner image present, model reachable.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

IMAGE="${1:-pillbox-runner:dev}"
MODEL="${2:-${SMOKE_MODEL:-zai-coding-plan/glm-4.5-air}}"
BACKEND="${PILLBOX_BACKEND:-libkrun}"
PB="$(pwd)/target/debug/pillbox"
export PILLBOX_BACKEND="$BACKEND" PILLBOX_RUNNER_IMAGE="$IMAGE"

fail() { echo "  ✗ dispatch: $1"; exit 1; }

# libkrun-only today: dispatch's grader resolves each worker's *live* workspace
# via `session info --json` → `.session.workspace`, which only libkrun sessions
# populate (`libkrun_workspace_path`). Docker dispatch needs a non-libkrun
# workspace-resolution path (pull-then-score, or score-by-result-snapshot) —
# deferred, see docs/dispatch.md. Skip (don't fail the suite) on docker.
if [ "$BACKEND" != libkrun ]; then
  echo "  · dispatch smoke skipped — backend=$BACKEND not wired yet (libkrun-only grade path)"
  exit 0
fi
[ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] \
  || fail "binary lacks the libkrun feature — run scripts/lk-build.sh"

WS="$(mktemp -d)"
RUBRIC="$(mktemp)"
ERR="/tmp/dispatch-smoke.err"
cleanup() {
  # Tear down every session dispatch left in this project (losers + winner),
  # then drop the throwaway project pillbox.
  if cd "$WS" 2>/dev/null; then
    for s in $("$PB" session list --json 2>/dev/null | jq -r '.sessions[].id' 2>/dev/null); do
      "$PB" session rm "$s" >/dev/null 2>&1
    done
    "$PB" rm dispatch-smoke >/dev/null 2>&1
  fi
  cd / && rm -rf "$WS" "$RUBRIC" "$ERR"
}
trap cleanup EXIT

# A trivial, deterministic task + a lenient 2-criterion rubric — the smoke tests
# the plumbing, not the model's cleverness, so the grade must be robust to LLM
# phrasing variation (contains DONE, not an exact-line match).
cat >"$RUBRIC" <<'EOF'
# NAME :: COMMAND (run in the graded workspace)
file-exists :: test -f result.txt
has-done :: grep -qi done result.txt
EOF

cd "$WS" || fail "cd into workspace $WS failed"
"$PB" new --name dispatch-smoke >/dev/null 2>&1 || fail "pillbox new failed"
echo seed >seed.txt
"$PB" push --bookmark base >/dev/null 2>&1 || fail "push --bookmark failed"

echo "▶ dispatch smoke (image=$IMAGE, k=2)"
OUT="$("$PB" dispatch --from-bookmark base -k 2 --rubric "$RUBRIC" \
  --agent opencode ${MODEL:+--model "$MODEL"} --json \
  -- 'Create a file named result.txt containing the single word: DONE' 2>"$ERR")"
RC=$?

echo "$OUT" | jq . >/dev/null 2>&1 \
  || { tail -6 "$ERR"; fail "verdict is not valid JSON: $OUT"; }

# Schema: {version:1, dispatch:{winner, workers[], pulled_to}}.
[ "$(echo "$OUT" | jq -r '.version')" = 1 ] || fail "verdict version != 1"
N="$(echo "$OUT" | jq -r '.dispatch.workers | length')"
[ "$N" -eq 2 ] || fail "expected 2 workers in the verdict, got $N"
echo "  ✓ 2 workers forked; verdict JSON matches the schema"

SCORED="$(echo "$OUT" | jq -r '[.dispatch.workers[] | select(.status=="scored")] | length')"
echo "  · $SCORED/2 workers scored"

WINNER="$(echo "$OUT" | jq -r '.dispatch.winner // empty')"
[ -n "$WINNER" ] || { echo "$OUT" | jq .; tail -6 "$ERR"; fail "no winner (rc=$RC) — no worker passed the rubric"; }
echo "  ✓ winner selected: $WINNER"

PULLED="$(echo "$OUT" | jq -r '.dispatch.pulled_to // empty')"
{ [ -n "$PULLED" ] && [ -d "$PULLED" ]; } || fail "winner not pulled (pulled_to=$PULLED)"
grep -qi 'done' "$PULLED/result.txt" 2>/dev/null \
  || fail "pulled winner workspace lacks result.txt with DONE — got: $(cat "$PULLED/result.txt" 2>/dev/null | tr '\n' ' ')"
echo "  ✓ winner pulled → result.txt recovered"

[ "$RC" -eq 0 ] || fail "winner found but exit code was $RC (want 0)"
echo "  ✓✓ dispatch PASS"
