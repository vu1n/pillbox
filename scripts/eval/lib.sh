# Shared helpers for the eval harness: the run→resolve→drive→wait spine over
# pillbox's CLI, single-sourcing the JSON-surface contract so a schema tweak (or
# a command rename) touches ONE place, not every caller. Sourced by run-task.sh
# and meta-harness/propose.sh. Functions read the caller's env: PILLBOX (binary),
# MAX_WAIT (idle-wait cap, seconds), MODEL (optional provider/modelID override).
# No side effects on source beyond defining functions.

# Start an opencode session against workspace $1; echo the 12-hex session id (or
# empty on failure, which the caller guards). MODEL overrides opencode's default.
pb_run_session() {
  "$PILLBOX" run --agent opencode --workspace "$1" ${MODEL:+--model "$MODEL"} 2>&1 \
    | grep -oE '[0-9a-f]{12}' | head -1
}

# Echo the host path of session $1's result-workspace (the agent's CoW clone),
# from the JSON surface — NOT by parsing the internal session record.
pb_workspace() {
  "$PILLBOX" session info "$1" --json 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["session"].get("workspace",""))'
}

# Drive session $1 with prompt $2, then block until the turn goes idle — the §0
# NeedsInput signal — draining the trajectory into the durable log meanwhile,
# capped by MAX_WAIT. A timeout is tolerated (the caller grades whatever landed).
pb_drive_and_wait() {
  "$PILLBOX" session send "$1" "$2" >/dev/null 2>&1
  "$PILLBOX" session wait-idle "$1" --timeout "$MAX_WAIT" >/dev/null 2>&1 || true
}
