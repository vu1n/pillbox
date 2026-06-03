# Shared helpers for the eval harness: the run→resolve→drive→wait spine over
# pillbox's CLI, single-sourcing the JSON-surface contract so a schema tweak (or
# a command rename) touches ONE place, not every caller. Sourced by run-task.sh
# and meta-harness/propose.sh. Functions read the caller's env: PILLBOX (binary),
# MAX_WAIT (idle-wait cap, seconds), MODEL (optional provider/modelID override).
# No side effects on source beyond defining functions.

# Start an opencode session against workspace $1; echo the 12-hex session id (or
# empty on failure, which the caller guards). MODEL overrides opencode's default.
#
# `--json` makes `run` emit `{version:1,session:{id,…}}` on stdout instead of the
# human banner, so we parse the id structurally — no `grep`-the-banner scrape.
# opencode is server-mode, so `run --json` is valid without `--detach` (the run
# persists a session record regardless).
pb_run_session() {
  "$PILLBOX" run --agent opencode --json --workspace "$1" ${MODEL:+--model "$MODEL"} 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["session"]["id"])
except Exception: pass'
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

# --- `session score --json` readers ---------------------------------------
# Single-source the verdict schema (field names like .criteria/.score/.passed)
# so a `score --json` change touches THIS file, not every orchestrator. Each
# reads the score JSON on stdin: `printf '%s' "$json" | pb_<reader>`. The loop
# *policy* (state→action, max-iter, feedback wording) stays in the caller.

# Plain --cmd verdict: echo `pass` or `fail` (fail on parse failure).
pb_passed() {
  python3 -c 'import json,sys
try: print("pass" if json.load(sys.stdin).get("passed") else "fail")
except Exception: print("fail")'
}

# The grader `feedback` string (empty on parse failure).
pb_feedback() {
  python3 -c 'import json,sys
try: sys.stdout.write(json.load(sys.stdin).get("feedback",""))
except Exception: pass'
}

# Rubric verdict classified from .criteria:
#   satisfied | needs_revision | rubric_failed (no criteria) | grader_error (unparseable)
pb_score_state() {
  python3 -c 'import json,sys
try: d=json.load(sys.stdin)
except Exception: print("grader_error"); sys.exit()
c=d.get("criteria",[])
print("rubric_failed" if not c else "satisfied" if all(x.get("passed") for x in c) else "needs_revision")'
}

# Numeric .score in [0,1] to 3dp (0 on parse failure).
pb_score_value() {
  python3 -c 'import json,sys
try: print("%.3f"%json.load(sys.stdin).get("score",0))
except Exception: print("0")'
}

# The .criteria array as JSON (`[]` on parse failure).
pb_criteria() {
  python3 -c 'import json,sys
try: print(json.dumps(json.load(sys.stdin).get("criteria",[])))
except Exception: print("[]")'
}

# A feedback message naming the FAILED criteria (.name + .feedback). The schema
# read is here; the caller decides whether/how to inject it.
pb_failed_feedback() {
  python3 -c 'import json,sys
d=json.load(sys.stdin); c=d.get("criteria",[])
fails=[x for x in c if not x.get("passed")]
out=["Your solution does not yet pass all checks (%d/%d). Fix ONLY these failing"
     " criteria, then stop and wait:"%(len(c)-len(fails),len(c))]
for x in fails:
    fb=(x.get("feedback") or "").strip()
    out.append("\n\n- %s%s"%(x["name"], (":\n"+fb) if fb else ""))
sys.stdout.write("".join(out))'
}
