#!/usr/bin/env bash
# Smoke test for the v0.7 events spike.
#
# Validates the event transport end-to-end with the smallest possible
# lifecycle: spawn a session, observe `session.started`, drop it,
# observe `session.dropped`. If this works, the JSONL stream + jq
# consumer pattern is good; if it doesn't, we have a bug to fix
# before iterating on the full productive-failure loop in run.sh.
#
# Prerequisites:
#   - PILLBOX_REMOTE  — registered e2b:// remote name
#   - jq              — JSON processing

set -euo pipefail

REMOTE="${PILLBOX_REMOTE:?must be set — register via \`pillbox remote add NAME e2b://TEMPLATE\`}"

EVENTS_LOG=$(mktemp "${TMPDIR:-/tmp}/pillbox-smoke-events-XXXXXX.log")
trap 'rm -f "$EVENTS_LOG"' EXIT

# Tail events in the background, write to a log we'll read back.
pillbox session events --follow --json >"$EVENTS_LOG" &
TAIL_PID=$!
trap 'kill "$TAIL_PID" 2>/dev/null || true; rm -f "$EVENTS_LOG"' EXIT

sleep 0.5  # let the tail attach

# Spawn the session
echo "▶ Spawning session..."
SESSION=$(pillbox run \
	--remote "$REMOTE" \
	--detach \
	--label "smoke-test" \
	--json -- "echo hello && sleep 30" | jq -r '.session.id')
echo "  → $SESSION"

# Wait briefly for the start event to land
sleep 1.5

# Drop it (this is the second event we want to see)
echo "▶ Dropping session..."
pillbox session rm "$SESSION" >/dev/null
sleep 1.0

# Now grade the events we captured
echo "▶ Events captured:"
jq -c "select(.session_id == \"$SESSION\")" "$EVENTS_LOG" | while IFS= read -r LINE; do
	echo "    $LINE"
done

SAW_STARTED=$(jq -c "select(.session_id == \"$SESSION\" and .event == \"session.started\")" "$EVENTS_LOG" | wc -l | tr -d ' ')
SAW_DROPPED=$(jq -c "select(.session_id == \"$SESSION\" and .event == \"session.dropped\")" "$EVENTS_LOG" | wc -l | tr -d ' ')

PASS=0
# Plain if/else (not `A && B || C`) — `echo` could theoretically fail
# (closed stdout, EIO) and silently flip the assertion. Shellcheck SC2015.
if [[ "$SAW_STARTED" -ge 1 ]]; then
	echo "  ✓ session.started observed"
else
	echo "  ✗ session.started MISSING"
	PASS=1
fi
if [[ "$SAW_DROPPED" -ge 1 ]]; then
	echo "  ✓ session.dropped observed"
else
	echo "  ✗ session.dropped MISSING"
	PASS=1
fi

exit "$PASS"
