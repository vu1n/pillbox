#!/usr/bin/env bash
# Live e2e smoke for `pillbox session guard` (the §0 circuit-breaker): start a
# detached run → guard its live stream with a 1-token budget → assert the trip
# fires. Unit tests cover the pure detector POLICY (repeat / error-spiral /
# token signals); this covers the subscribe→detect→kill WIRING on a booted VM
# (the trip→`session rm` kill path the detector can't reach).
#
# The trip is made DETERMINISTIC via the token detector: any real run emits a
# `Usage` event early, so `--max-tokens 1` trips on the first one. Two arms:
#   1. --kill   → assert the session is GONE from `session list`.
#   2. (default) → assert a trip is LOGGED and the session STILL exists.
#
#   Usage: scripts/smoke/session-guard.sh [runner-image] [model]
# Env: SMOKE_MODEL (model).
# Prereqs: codesigned libkrun binary (scripts/lk-build.sh), opencode authed
# (`pillbox auth login --agent opencode`), the runner image present, model reachable.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

IMAGE="${1:-pillbox-runner:dev}"
MODEL="${2:-${SMOKE_MODEL:-zai-coding-plan/glm-4.5-air}}"
BACKEND="${PILLBOX_BACKEND:-libkrun}"
PB="$(pwd)/target/debug/pillbox"
export PILLBOX_BACKEND="$BACKEND" PILLBOX_RUNNER_IMAGE="$IMAGE"

fail() { echo "  ✗ session-guard: $1"; exit 1; }

# libkrun-only today: `session guard` resolves + kills a live session via the
# LiveSession plane; the smoke needs a real booted VM. Skip (don't fail the
# suite) on docker.
if [ "$BACKEND" != libkrun ]; then
  echo "  · session-guard smoke skipped — backend=$BACKEND not exercised (libkrun-only)"
  exit 0
fi
[ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] \
  || fail "binary lacks the libkrun feature — run scripts/lk-build.sh"

WS="$(mktemp -d)"
cleanup() {
  # Tear down any session this smoke left behind (the dry-run arm leaves one
  # alive on purpose), then drop the throwaway project pillbox.
  if cd "$WS" 2>/dev/null; then
    for s in $("$PB" session list --json 2>/dev/null | jq -r '.sessions[].id' 2>/dev/null); do
      "$PB" session rm "$s" >/dev/null 2>&1
    done
    "$PB" rm guard-smoke >/dev/null 2>&1
  fi
  cd / && rm -rf "$WS"
}
trap cleanup EXIT

cd "$WS" || fail "cd into workspace $WS failed"
"$PB" new --name guard-smoke >/dev/null 2>&1 || fail "pillbox new failed"

# Detached mode is spin-up-THEN-drive: `run --detach` returns a booted, IDLE
# session; a turn runs only when you `session send` (the contract dispatch relies
# on). So the guard must be SUBSCRIBED first, then the turn driven, so the turn's
# Usage event lands while the guard is watching (the guard reads from the live
# head, not history). A 1-token budget then trips on that first Usage. Without the
# send the session is silent and an event-driven guard correctly has nothing to
# react to — that is not a guard bug, it's an undriven session.
TASK='Print the single word: DONE'

# Detach only — no prompt; the turn is driven by `session send` in guard_and_drive.
start_session() {
  "$PB" run --detach --json --agent opencode ${MODEL:+--model "$MODEL"} 2>/dev/null \
    | jq -r '.session.id // empty'
}

session_exists() {
  "$PB" session list --json 2>/dev/null | jq -e --arg id "$1" \
    '.sessions[] | select(.id==$id)' >/dev/null 2>&1
}

# Background `session guard <sid> <args…>` (it subscribes from the live head),
# give it a beat to attach, then drive a turn so a Usage event flows while it
# watches. BOUNDED wait (≤90s) so a quiet session can never hang the smoke — the
# 71-min orphan lesson. Echoes the captured guard output.
guard_and_drive() {
  local sid="$1"; shift
  local gout; gout="$(mktemp)"
  "$PB" session guard "$sid" "$@" >"$gout" 2>&1 &
  local gpid=$!
  sleep 2                                                    # let the guard attach
  "$PB" session send "$sid" "$TASK" >/dev/null 2>&1 || true  # drive the turn (may race the kill — fine)
  for _ in $(seq 1 45); do kill -0 "$gpid" 2>/dev/null || break; sleep 2; done
  kill "$gpid" 2>/dev/null || true; wait "$gpid" 2>/dev/null || true
  cat "$gout"; rm -f "$gout"
}

# ── Arm 1: --kill trips on the token budget and tears the session down ────────
echo "▶ session-guard smoke (image=$IMAGE) — --kill arm"
SID="$(start_session)"
[ -n "$SID" ] || fail "run --detach --json produced no session id"
echo "  · started session $SID"
OUT="$(guard_and_drive "$SID" --max-tokens 1 --kill)"
echo "$OUT" | grep -qi 'guard tripped' || { echo "$OUT"; fail "guard did not trip on a 1-token budget"; }
echo "$OUT" | grep -qi 'killed session' || { echo "$OUT"; fail "armed guard tripped but did not report a kill"; }
session_exists "$SID" && fail "session $SID still present after a --kill trip"
echo "  ✓ --kill: tripped + session torn down"

# ── Arm 2: default (dry-run) trips + LOGS but leaves the session alive ────────
echo "▶ session-guard smoke — dry-run arm"
SID="$(start_session)"
[ -n "$SID" ] || fail "run --detach --json produced no session id (arm 2)"
echo "  · started session $SID"
OUT="$(guard_and_drive "$SID" --max-tokens 1)"
echo "$OUT" | grep -qi 'guard tripped' || { echo "$OUT"; fail "dry-run guard did not trip"; }
echo "$OUT" | grep -qi 'would kill' || { echo "$OUT"; fail "dry-run guard tripped but did not log the would-kill notice"; }
session_exists "$SID" || fail "dry-run trip removed session $SID (must leave it alive)"
echo "  ✓ dry-run: tripped + logged + session left alive"

echo "  ✓✓ session-guard PASS"
