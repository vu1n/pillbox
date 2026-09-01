#!/usr/bin/env bash
# Live end-to-end smoke for one libkrun server-mode agent — the guard against
# "unit-green, live-broken" regressions that CI can't reach (it's unit-only; no
# libkrun, no live agent). Exercises the whole spine: boot → drive a turn → assert
# §0 token-usage + agent-output events flowed → `session pull` recovers the edit
# from the live workspace → original untouched (fork-from-store) → teardown.
# (This is exactly the path the opencode CA regression broke while CI stayed green.)
#
#   Usage: scripts/smoke/libkrun.sh <agent> <runner-image> [model]
# Env: SMOKE_TIMEOUT (idle wait, default 240s).
# Prereqs: a codesigned libkrun binary (scripts/lk-build.sh), the agent authed
# (`pillbox auth login --agent …`), the runner image present, model reachable.
set -uo pipefail
cd "$(dirname "$0")/../.."

AGENT="${1:?usage: libkrun.sh <agent> <runner-image> [model]}"
IMAGE="${2:?usage: libkrun.sh <agent> <runner-image> [model]}"
MODEL="${3:-}"
PB=./target/debug/pillbox
export PILLBOX_BACKEND=libkrun PILLBOX_RUNNER_IMAGE="$IMAGE"

fail() { echo "  ✗ $AGENT: $1"; exit 1; }

# The binary must be the codesigned libkrun build, or this tests nothing.
# (grep -c, not -q: under `pipefail`, grep -q short-circuits → the producer gets
# SIGPIPE → the pipeline reports non-zero even on a match. grep -c reads to EOF.)
[ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] \
  || fail "binary lacks the libkrun feature — run scripts/lk-build.sh"
[ "$(codesign -d --entitlements :- "$PB" 2>/dev/null | grep -c hypervisor)" -ge 1 ] \
  || fail "binary not codesigned — run scripts/lk-build.sh"

WS=$(mktemp -d)
PULLED=$(mktemp -d)
SID=""
cleanup() {
  [ -n "$SID" ] && $PB session rm "$SID" >/dev/null 2>&1
  rm -rf "$WS" "$PULLED" "/tmp/smoke-$AGENT.err"
}
trap cleanup EXIT

# A mechanical, unambiguous edit so "did the pull recover the change" is a
# reliable gate (we test the plumbing, not the model's cleverness).
printf 'def f(a, b):\n    return a - b\n' >"$WS/bug.py"

echo "▶ $AGENT smoke (image=$IMAGE)"
SID=$($PB run --agent "$AGENT" --json --workspace "$WS" ${MODEL:+--model "$MODEL"} \
  2>"/tmp/smoke-$AGENT.err" | jq -r '.session.id // empty')
[ -n "$SID" ] || { tail -4 "/tmp/smoke-$AGENT.err"; fail "run did not start a session"; }
echo "  ✓ started $SID"

$PB session send "$SID" 'In bug.py, change `return a - b` to `return a + b`. Edit the file.' \
  >/dev/null 2>&1 || fail "session send failed"
$PB session wait-idle "$SID" --timeout "${SMOKE_TIMEOUT:-240}" >/dev/null 2>&1 \
  || fail "turn never reached idle within ${SMOKE_TIMEOUT:-240}s"
echo "  ✓ drove a turn → idle"

usage=$($PB session log "$SID" --type usage 2>/dev/null | grep -c '"type":"usage"')
[ "${usage:-0}" -ge 1 ] || fail "no §0 usage events — the token-usage producer is broken"
echo "  ✓ §0 usage events: $usage"

msgs=$($PB session log "$SID" --type message_delta 2>/dev/null | grep -c '"type":"message_delta"')
[ "${msgs:-0}" -ge 1 ] || fail "no agent output in §0 — the drain is broken"
echo "  ✓ §0 agent output: $msgs deltas"

$PB session pull "$SID" --to "$PULLED" >/dev/null 2>&1 || fail "session pull failed"
grep -q 'return a + b' "$PULLED/bug.py" 2>/dev/null \
  || fail "pull did not recover the edit — got: $(cat "$PULLED/bug.py" 2>/dev/null | tr '\n' ' ')"
echo "  ✓ session pull recovered the edit"

grep -q 'return a - b' "$WS/bug.py" \
  || fail "the ORIGINAL workspace was mutated — fork-from-store violated"
echo "  ✓ original workspace untouched (fork-from-store)"
echo "  ✓✓ $AGENT PASS"
