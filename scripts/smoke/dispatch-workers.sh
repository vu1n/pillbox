#!/usr/bin/env bash
# Live e2e smoke for `pillbox dispatch --workers-spec` (the heterogeneous worker
# roster): fork k workers — each bound to its own roster row (agent/model/
# temperature) — from a bookmark → drive → score → select → pull. Modeled on
# scripts/smoke/dispatch.sh; here the WIRING under test is the roster (k derived
# from the roster length, per-worker argv) on booted VMs, which the MockDriver
# unit tests can't reach.
#
#   Usage: scripts/smoke/dispatch-workers.sh [runner-image] [model]
# Env: SMOKE_MODEL (model both rostered workers use), PILLBOX_DISPATCH_TURN_TIMEOUT.
# Prereqs: codesigned libkrun binary (scripts/lk-build.sh), opencode authed
# (`pillbox auth login --agent opencode`), the runner image present, model reachable.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

IMAGE="${1:-pillbox-runner:dev}"
MODEL="${2:-${SMOKE_MODEL:-zai-coding-plan/glm-4.5-air}}"
BACKEND="${PILLBOX_BACKEND:-libkrun}"
PB="$(pwd)/target/debug/pillbox"
export PILLBOX_BACKEND="$BACKEND" PILLBOX_RUNNER_IMAGE="$IMAGE"

fail() { echo "  ✗ dispatch-workers: $1"; exit 1; }

# libkrun-only today (same grade-path constraint as dispatch.sh): the grader
# resolves each worker's live workspace via `session info --json` →
# `.session.workspace`, which only libkrun sessions populate. Skip on docker.
if [ "$BACKEND" != libkrun ]; then
  echo "  · dispatch-workers smoke skipped — backend=$BACKEND not wired yet (libkrun-only grade path)"
  exit 0
fi
[ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] \
  || fail "binary lacks the libkrun feature — run scripts/lk-build.sh"

WS="$(mktemp -d)"
RUBRIC="$(mktemp)"
SPEC="$(mktemp)"
ERR="/tmp/dispatch-workers-smoke.err"
cleanup() {
  # Tear down every session dispatch left in this project (losers + winner),
  # then drop the throwaway project pillbox.
  if cd "$WS" 2>/dev/null; then
    for s in $("$PB" session list --json 2>/dev/null | jq -r '.sessions[].id' 2>/dev/null); do
      "$PB" session rm "$s" >/dev/null 2>&1
    done
    "$PB" rm dispatch-workers-smoke >/dev/null 2>&1
  fi
  cd / && rm -rf "$WS" "$RUBRIC" "$SPEC" "$ERR"
}
trap cleanup EXIT

# A trivial, deterministic task + a lenient 2-criterion rubric — the smoke tests
# the roster plumbing, not the model's cleverness (contains DONE, not exact-line).
cat >"$RUBRIC" <<'EOF'
# NAME :: COMMAND (run in the graded workspace)
file-exists :: test -f result.txt
has-done :: grep -qi done result.txt
EOF

# A 2-entry roster (both opencode + the same model so the smoke is reproducible
# with one set of creds; the heterogeneity under test is that the roster supplies
# k=2 and binds each worker's argv per row).
# Build the model line in a var: the TOML quotes must be literal text, not inside
# a ${MODEL:+...} expansion (shell quote-removal there would emit an UNQUOTED, and
# thus invalid, TOML value). Empty SMOKE_MODEL → no line → workers use the agent default.
MODEL_LINE=""
[ -n "$MODEL" ] && MODEL_LINE="model = \"$MODEL\""
cat >"$SPEC" <<EOF
[[worker]]
agent = "opencode"
$MODEL_LINE

[[worker]]
agent = "opencode"
$MODEL_LINE
temperature = 0.7
EOF

cd "$WS" || fail "cd into workspace $WS failed"
"$PB" new --name dispatch-workers-smoke >/dev/null 2>&1 || fail "pillbox new failed"
echo seed >seed.txt
"$PB" push --bookmark base >/dev/null 2>&1 || fail "push --bookmark failed"

echo "▶ dispatch-workers smoke (image=$IMAGE, roster k=2)"
# No -k: the roster length (2) is authoritative.
OUT="$("$PB" dispatch --from-bookmark base --workers-spec "$SPEC" --rubric "$RUBRIC" --json \
  -- 'Create a file named result.txt containing the single word: DONE' 2>"$ERR")"
RC=$?

echo "$OUT" | jq . >/dev/null 2>&1 \
  || { tail -6 "$ERR"; fail "verdict is not valid JSON: $OUT"; }

# Schema: {version:1, dispatch:{winner, workers[], pulled_to}}.
[ "$(echo "$OUT" | jq -r '.version')" = 1 ] || fail "verdict version != 1"
N="$(echo "$OUT" | jq -r '.dispatch.workers | length')"
[ "$N" -eq 2 ] || fail "expected 2 workers (roster-derived k), got $N"
echo "  ✓ roster supplied k=2; verdict JSON matches the schema"

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
echo "  ✓✓ dispatch-workers PASS"
