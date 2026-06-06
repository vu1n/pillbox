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

# --- cost / usage reader (the cost-adjusted-quality metric's denominator) -----
# Fold a session's durable §0 `usage` events into total tokens + a $cost, read
# via `session log <id> --type usage` (one Event JSON per line; payload fields
# are camelCase). The log already applies wire/native source precedence at
# emission — exactly one source's usage per message — so summing every event
# never double-counts. Prices are per-1M-token env overrides (default to Claude
# Sonnet list; set them to the eval model's rate). Emits compact JSON:
#   {"input":N,"output":N,"cacheRead":N,"cacheCreation":N,"costUsd":F}
# Call: pb_usage <session-id>
pb_usage() {
  "$PILLBOX" session log "$1" --type usage 2>/dev/null | python3 -c '
import json, os, sys
pin  = float(os.environ.get("PRICE_IN_PER_M", "3.0"))
pout = float(os.environ.get("PRICE_OUT_PER_M", "15.0"))
pcr  = float(os.environ.get("PRICE_CACHE_READ_PER_M", "0.30"))
pcc  = float(os.environ.get("PRICE_CACHE_CREATION_PER_M", "3.75"))
i=o=cr=cc=0
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    try: p=json.loads(line)["payload"]
    except Exception: continue
    if p.get("type")!="usage": continue
    i  += p.get("inputTokens") or 0
    o  += p.get("outputTokens") or 0
    cr += p.get("cacheReadInputTokens") or 0
    cc += p.get("cacheCreationInputTokens") or 0
cost = (i*pin + o*pout + cr*pcr + cc*pcc)/1_000_000
print(json.dumps({"input":i,"output":o,"cacheRead":cr,"cacheCreation":cc,"costUsd":round(cost,6)}))
'
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
