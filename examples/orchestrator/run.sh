#!/usr/bin/env bash
# Tiny orchestrator — productive-failure loop driver.
#
# Spawns a pillbox session with an ambitious task, watches for failure,
# and on failure spawns an analyzer session pointed at the failed fork.
#
# Prerequisites:
#   - PILLBOX_REMOTE  — name of a registered e2b:// remote (set up via
#                       `pillbox remote add NAME e2b://TEMPLATE`)
#   - jq              — JSON processing
#   - pillbox v0.7+   — events + pull primitive

set -euo pipefail

# ────────── Args
TASK="${1:-}"
if [[ -z "$TASK" ]]; then
	echo "usage: $0 <task-prompt>" >&2
	exit 64
fi

REMOTE="${PILLBOX_REMOTE:-}"
if [[ -z "$REMOTE" ]]; then
	echo "error: PILLBOX_REMOTE not set" >&2
	echo "  register one with: pillbox remote add NAME e2b://TEMPLATE_ID" >&2
	exit 64
fi

# Webhook URL the sandbox-side `pillbox session done` POSTs to. The
# orchestrator IS the listener — without this, terminal events
# (completed/failed) never reach back from a detached sandbox. The
# orchestrator script (in a real impl) would run a small HTTP server on
# this URL; the spec here just plumbs it through.
WEBHOOK_URL="${PILLBOX_EVENTS_WEBHOOK:-}"

REPORT_DIR="${PILLBOX_REPORT_DIR:-./reports}"
mkdir -p "$REPORT_DIR"

# ────────── Stage 1+2: ambitious task
echo "▶ Spawning ambitious task: $TASK"
TASK_SESSION=$(pillbox run \
	--remote "$REMOTE" \
	--detach \
	--label "task: ${TASK:0:60}" \
	${WEBHOOK_URL:+--events-webhook "$WEBHOOK_URL"} \
	--json -- "$TASK" | jq -r '.session.id')
echo "  → session: $TASK_SESSION"

# ────────── Stage 3: wait for completion or failure
echo "▶ Watching session events..."

# Subscribe to events filtered to this session. The --follow stream
# keeps going until the session reaches a terminal state, then we read
# the outcome out of the matched line.
OUTCOME_LINE=""
while IFS= read -r EVENT; do
	EVENT_TYPE=$(echo "$EVENT" | jq -r '.event')
	case "$EVENT_TYPE" in
	session.started)
		echo "  ✓ started at $(echo "$EVENT" | jq -r '.started_at')"
		;;
	session.completed)
		echo "  ✓ completed cleanly — no failure to analyze"
		OUTCOME_LINE="$EVENT"
		break
		;;
	session.failed)
		echo "  ✗ failed: $(echo "$EVENT" | jq -r '.reason // "unknown"')"
		OUTCOME_LINE="$EVENT"
		break
		;;
	esac
done < <(pillbox session events --follow --filter "session_id=$TASK_SESSION" --json)

OUTCOME=$(echo "$OUTCOME_LINE" | jq -r '.event')

# Happy path: nothing to do, the task succeeded
if [[ "$OUTCOME" == "session.completed" ]]; then
	pillbox session rm "$TASK_SESSION" >/dev/null
	exit 0
fi

# ────────── Stage 4: second-pass analysis
echo "▶ Pulling failed fork for analysis..."
FAILED_DIR=$(mktemp -d "${TMPDIR:-/tmp}/pillbox-failed-XXXXXX")
# The orchestrator listening on `$WEBHOOK_URL` should have already
# called `pillbox session done --result-snapshot HANDLE` on the host,
# so the record carries the snapshot. If you're running this without
# a webhook listener, pass `--result-snapshot HANDLE` straight from
# the event payload before invoking `session pull`.
pillbox session pull "$TASK_SESSION" --to "$FAILED_DIR"

# Trace path (if pillbox attached one) goes alongside the workspace
TRACE_PATH=$(echo "$OUTCOME_LINE" | jq -r '.trace_path // empty')
TRACE_HINT=""
if [[ -n "$TRACE_PATH" ]]; then
	TRACE_HINT="The agent's tool-call trace is at $TRACE_PATH."
fi

ANALYZER_PROMPT=$(
	cat <<-EOF
		The previous run of this task failed:

		  Task: $TASK
		  Reason: $(echo "$OUTCOME_LINE" | jq -r '.reason // "unknown"')

		The failed run's workspace is at /failed (read-only). $TRACE_HINT

		Read /failed carefully. Then answer ONE question:
		  → What missing capability would have let the agent finish?

		Be concrete: name the tool, context, or prompt change that closes
		the gap. Save the answer to /workspace/MISSING_CAPABILITY.md.
	EOF
)

echo "▶ Spawning analyzer..."
ANALYZER_SESSION=$(pillbox run \
	--remote "$REMOTE" \
	--detach \
	--mount "$FAILED_DIR:/failed:ro" \
	--label "analyze: ${TASK:0:50}" \
	${WEBHOOK_URL:+--events-webhook "$WEBHOOK_URL"} \
	--json -- "$ANALYZER_PROMPT" | jq -r '.session.id')
echo "  → analyzer: $ANALYZER_SESSION"

# Wait for the analyzer
while IFS= read -r EVENT; do
	EVENT_TYPE=$(echo "$EVENT" | jq -r '.event')
	if [[ "$EVENT_TYPE" == "session.completed" || "$EVENT_TYPE" == "session.failed" ]]; then
		break
	fi
done < <(pillbox session events --follow --filter "session_id=$ANALYZER_SESSION" --json)

# ────────── Stage 5: surface the report
REPORT="$REPORT_DIR/${TASK_SESSION}-analysis.md"
pillbox session pull "$ANALYZER_SESSION" --to "${FAILED_DIR}.analyzed"
if [[ -f "${FAILED_DIR}.analyzed/MISSING_CAPABILITY.md" ]]; then
	cp "${FAILED_DIR}.analyzed/MISSING_CAPABILITY.md" "$REPORT"
	echo ""
	echo "════════════════════════════════════════════"
	echo " Missing capability report → $REPORT"
	echo "════════════════════════════════════════════"
	cat "$REPORT"
else
	echo "  (analyzer didn't write MISSING_CAPABILITY.md — see $FAILED_DIR.analyzed)" >&2
fi

# ────────── Cleanup
pillbox session rm "$TASK_SESSION" >/dev/null
pillbox session rm "$ANALYZER_SESSION" >/dev/null

# Non-zero exit so CI / cron loops can detect the failure cycle
exit 1
