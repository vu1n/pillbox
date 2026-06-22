#!/usr/bin/env bash
# Live e2e smoke for `pillbox dispatch --segments` (the GHOST-004 analog for the
# segment chain): fork ONE worker from a bookmark → drive an ordered chain of
# focused, gate-checkpointed sub-prompts SEQUENTIALLY in ONE session (context
# accumulates, the horizon never resets) → grade each segment by its gate →
# score the whole chain by the run-level reward → select → pull. Unit tests cover
# the chain/gate/retry POLICY; this covers the drive/gate/reward WIRING on a
# booted VM. k=1: the chain is what's under test (fork-`k` diversity is covered by
# dispatch.sh), and k=1 boots just one VM, keeping it cheap.
#
#   Usage: scripts/smoke/dispatch-segments.sh [runner-image] [model]
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

fail() { echo "  ✗ dispatch-segments: $1"; exit 1; }

# libkrun-only today: dispatch's grader resolves each worker's *live* workspace
# via `session info --json` → `.session.workspace`, which only libkrun sessions
# populate (`libkrun_workspace_path`). Docker dispatch needs a non-libkrun
# workspace-resolution path — deferred, see docs/dispatch.md. Skip (don't fail
# the suite) on docker.
if [ "$BACKEND" != libkrun ]; then
  echo "  · dispatch-segments smoke skipped — backend=$BACKEND not wired yet (libkrun-only grade path)"
  exit 0
fi
[ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] \
  || fail "binary lacks the libkrun feature — run scripts/lk-build.sh"

WS="$(mktemp -d)"
SPEC="$(mktemp)"
REWARD="$(mktemp)"
ERR="/tmp/dispatch-segments-smoke.err"
cleanup() {
  # Tear down every session dispatch left in this project (just the one worker),
  # then drop the throwaway project pillbox.
  if cd "$WS" 2>/dev/null; then
    for s in $("$PB" session list --json 2>/dev/null | jq -r '.sessions[].id' 2>/dev/null); do
      "$PB" session rm "$s" >/dev/null 2>&1
    done
    "$PB" rm dispatch-segments-smoke >/dev/null 2>&1
  fi
  cd / && rm -rf "$WS" "$SPEC" "$REWARD" "$ERR"
}
trap cleanup EXIT

# A self-contained 2-segment toy chain: each segment's gate runs against the
# worker's live workspace as-is (NO hidden tests), so the checks live in the
# gate commands themselves. Inline prompt + gate_cmd — no prompt_file needed.
# The grade must be robust to LLM phrasing variation (grep -qi, not exact-line).
cat >"$SPEC" <<'EOF'
[[segment]]
name = "alpha"
prompt = "Create a file named a.txt containing the single word: ALPHA"
gate_cmd = "test -f a.txt && grep -qi alpha a.txt"

[[segment]]
name = "beta"
prompt = "Now also create a file named b.txt containing the single word: BETA. Keep a.txt as it is."
gate_cmd = "test -f b.txt && grep -qi beta b.txt"
EOF

# The run-level reward (the authoritative final grade that selects the winner) —
# both files must survive the chain.
cat >"$REWARD" <<'EOF'
# NAME :: COMMAND (run in the graded workspace)
has-a :: grep -qi alpha a.txt
has-b :: grep -qi beta b.txt
EOF

cd "$WS" || fail "cd into workspace $WS failed"
"$PB" new --name dispatch-segments-smoke >/dev/null 2>&1 || fail "pillbox new failed"
echo seed >seed.txt
"$PB" push --bookmark base >/dev/null 2>&1 || fail "push --bookmark failed"

echo "▶ dispatch-segments smoke (image=$IMAGE, k=1, 2 segments)"
# No positional prompt: the segments carry the work — a valid invocation in
# segments mode.
OUT="$("$PB" dispatch --from-bookmark base --segments "$SPEC" -k 1 --rubric "$REWARD" \
  --agent opencode ${MODEL:+--model "$MODEL"} --json -- 2>"$ERR")"
RC=$?

echo "$OUT" | jq . >/dev/null 2>&1 \
  || { tail -6 "$ERR"; fail "verdict is not valid JSON: $OUT"; }

# Schema: {version:1, dispatch:{winner, workers[{segments[]}], pulled_to}}.
[ "$(echo "$OUT" | jq -r '.version')" = 1 ] || fail "verdict version != 1"
N="$(echo "$OUT" | jq -r '.dispatch.workers | length')"
[ "$N" -eq 1 ] || { echo "$OUT" | jq .; fail "expected 1 worker in the verdict, got $N"; }
echo "  ✓ 1 worker forked; verdict JSON matches the schema"

# The segments array is present ONLY for a --segments worker — assert the chain.
SEGN="$(echo "$OUT" | jq -r '.dispatch.workers[0].segments | length')"
[ "$SEGN" -eq 2 ] || { echo "$OUT" | jq .; fail "expected 2 segments in the worker, got $SEGN"; }
echo "  ✓ 2 segments recorded in the worker trajectory"

SEG_OK="$(echo "$OUT" | jq -r '[.dispatch.workers[0].segments[] | select(.passed==true)] | length')"
[ "$SEG_OK" -eq 2 ] \
  || { echo "$OUT" | jq '.dispatch.workers[0].segments'; tail -6 "$ERR"; fail "expected both segment gates to pass, got $SEG_OK/2"; }
echo "  ✓ both segment gates passed"

WINNER="$(echo "$OUT" | jq -r '.dispatch.winner // empty')"
[ -n "$WINNER" ] || { echo "$OUT" | jq .; tail -6 "$ERR"; fail "no winner (rc=$RC) — the chain failed the run-level reward"; }
echo "  ✓ winner selected: $WINNER"

PULLED="$(echo "$OUT" | jq -r '.dispatch.pulled_to // empty')"
{ [ -n "$PULLED" ] && [ -d "$PULLED" ]; } || fail "winner not pulled (pulled_to=$PULLED)"
grep -qi 'alpha' "$PULLED/a.txt" 2>/dev/null \
  || fail "pulled winner workspace lacks a.txt with ALPHA — got: $(cat "$PULLED/a.txt" 2>/dev/null | tr '\n' ' ')"
grep -qi 'beta' "$PULLED/b.txt" 2>/dev/null \
  || fail "pulled winner workspace lacks b.txt with BETA — got: $(cat "$PULLED/b.txt" 2>/dev/null | tr '\n' ' ')"
echo "  ✓ winner pulled → a.txt (ALPHA) + b.txt (BETA) recovered"

[ "$RC" -eq 0 ] || fail "winner found but exit code was $RC (want 0)"
echo "  ✓✓ dispatch-segments PASS"
