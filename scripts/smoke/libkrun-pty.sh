#!/usr/bin/env bash
# Live PTY-DRIVE smoke for the libkrun backend (the substrate-plane Phase 4
# keystone). The server-mode smoke (libkrun.sh) can't reach this path: it
# exercises both NEW PTY halves on a real microVM —
#   • send       — pty_send: host → persistent attach socket → guest PTY
#   • live read  — event_source: the creds_share transcript → the durable §0 log
#                  + wait-idle (the turn-done idle signal derived from the transcript)
# CI is unit-only (no libkrun, no live agent), so this is the only guard against
# "unit-green, live-broken" for the IDE drive+read loop.
#
#   Usage: scripts/smoke/libkrun-pty.sh [agent] [runner-image]   (default: claude pillbox-runner:l7)
# Env: SMOKE_TIMEOUT (per-turn idle wait, default 240s).
# Prereqs: a codesigned libkrun binary (scripts/lk-build.sh), the agent authed
#          (`pillbox auth login --agent …`), the runner image present.
set -uo pipefail
cd "$(dirname "$0")/../.."

AGENT="${1:-claude}"
IMAGE="${2:-pillbox-runner:l7}"
TIMEOUT="${SMOKE_TIMEOUT:-240}"
PB=./target/debug/pillbox
export PILLBOX_BACKEND=libkrun PILLBOX_RUNNER_IMAGE="$IMAGE"

fail() { echo "  ✗ pty-smoke($AGENT): $1"; exit 1; }

# Must be the codesigned libkrun build, or this tests nothing (it'd silently fall
# back to docker). grep -c (not -q) so `pipefail` doesn't trip on SIGPIPE.
[ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] \
  || fail "binary lacks the libkrun feature — run scripts/lk-build.sh"
[ "$(codesign -d --entitlements :- "$PB" 2>/dev/null | grep -c hypervisor)" -ge 1 ] \
  || fail "binary not codesigned — run scripts/lk-build.sh"

WS=$(mktemp -d); echo "pty smoke workspace" > "$WS/README.txt"
SID=""
cleanup() { [ -n "$SID" ] && $PB session rm "$SID" >/dev/null 2>&1; rm -rf "$WS"; }
trap cleanup EXIT

log_has() { $PB session log "$SID" 2>/dev/null | grep -q "$1"; }

echo "== launch detached PTY $AGENT (seeded turn 1) =="
SID=$($PB run --agent "$AGENT" --detach --json --workspace "$WS" \
        -- "Reply with exactly: SMOKE_TAIL_OK and nothing else." 2>/tmp/pty-smoke.err \
      | python3 -c "import sys,json;print(json.load(sys.stdin)['session']['id'])") \
  || fail "launch failed: $(tail -3 /tmp/pty-smoke.err)"
[ -n "$SID" ] || fail "launch produced no session id"
echo "  session $SID"

# READ HALF: drain turn 1 into the §0 log (rc ignored — the drain runs regardless
# of whether wait-idle catches turn 1's idle, which can race the launch return),
# then assert the seeded response landed via the creds_share transcript tailer.
echo "== read half: turn-1 transcript → §0 log =="
$PB session wait-idle "$SID" --timeout "$TIMEOUT" >/dev/null 2>&1 || true
log_has "SMOKE_TAIL_OK" \
  || fail "turn-1 response not in §0 log — the creds_share transcript tailer didn't capture it"
echo "  ✓ event_source PTY tail: turn-1 response in the §0 log"

# SEND HALF + IDLE SIGNAL: drive a second turn over pty_send, then REQUIRE wait-idle
# to fire (rc 0) — proving both the byte delivery and the turn-done signal.
echo "== send half: pty_send turn 2 + wait-idle =="
$PB session send "$SID" "Reply with exactly: SMOKE_SEND_OK and nothing else." >/dev/null 2>&1 \
  || fail "session send failed"
$PB session wait-idle "$SID" --timeout "$TIMEOUT" >/dev/null 2>&1 \
  || fail "wait-idle (turn 2) timed out — the idle signal isn't being derived"
log_has "SMOKE_SEND_OK" \
  || fail "turn-2 response not in §0 log — pty_send didn't reach the guest PTY"
echo "  ✓ pty_send delivered + turn-2 response tailed + wait-idle fired"

echo "== teardown (attributed killpg) =="
$PB session rm "$SID" >/dev/null 2>&1 || fail "session rm failed"
SID=""
echo "  ✓ session removed"

echo "✓✓✓ PTY SMOKE PASS ($AGENT)"
