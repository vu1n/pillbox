#!/usr/bin/env bash
# The meta-harness's self-improvement step: reflect on eval FAILURES and write an
# improved instruction profile. Runs THROUGH pillbox (an opencode "reflection"
# session) — reusing the same auth/egress/§0, no new model integration.
#
# Usage: propose.sh --faildir DIR --out FILE [--profile CURRENT]
#   --faildir : dir of per-task failure reports (run-task.sh FAILDIR= output:
#               task + what the agent produced + the grader feedback)
#   --out     : write the improved profile here
#   --profile : the current profile to refine (optional)
#
# Env: PILLBOX, PILLBOX_RUNNER_IMAGE, MODEL (use a capable model), MAX_WAIT.
set -euo pipefail
faildir="" out="" profile_in=""
while [ $# -gt 0 ]; do case "$1" in
  --faildir) faildir="$2"; shift 2;;
  --out) out="$2"; shift 2;;
  --profile) profile_in="$2"; shift 2;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done
: "${faildir:?--faildir required}"; : "${out:?--out required}"
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
MAX_WAIT="${MAX_WAIT:-160}"
export PILLBOX_BACKEND=libkrun

# The reflection workspace: the failures (+ the current profile) for the agent to
# read, and PROFILE.md for it to write.
ws="$(mktemp -d)"; trap 'rm -rf "$ws"' EXIT
mkdir -p "$ws/failures"
cp "$faildir"/*.md "$ws/failures/" 2>/dev/null || { echo "no failure reports in $faildir" >&2; exit 1; }
[ -n "$profile_in" ] && [ -f "$profile_in" ] && cp "$profile_in" "$ws/current_profile.md"
: > "$ws/PROFILE.md"

read -r -d '' TASK <<'EOF' || true
You are improving the INSTRUCTION PROFILE for a coding agent. The files in
failures/ are tasks the agent just FAILED — each shows the task, what the agent
produced, its TRAJECTORY (the tools it called, in order — how it approached the
problem), and the grader feedback (why it was rejected). Use the trajectory to
diagnose the PROCESS failure, not just the wrong output. current_profile.md, if
present, is the agent's current guidance.

Write an improved instruction profile into PROFILE.md: a short list of GENERAL
bullets that address the failure PATTERNS you see (e.g. output-shape mistakes,
missed edge cases, wrong interfaces), so the agent avoids them on future,
unseen tasks. Rules: keep it general — do NOT hardcode answers or task-specific
details; fold in the still-useful parts of current_profile.md; aim for under ~10
crisp bullets. Edit PROFILE.md only.
EOF

sid="$("$PILLBOX" run --agent opencode --workspace "$ws" ${MODEL:+--model "$MODEL"} 2>&1 | grep -oE '[0-9a-f]{12}' | head -1)"
[ -n "$sid" ] || { echo "propose: no session" >&2; exit 1; }
rec="$HOME/.pillbox/global/sessions/$sid.toml"
clone="$(python3 -c "import json,re;r=open('$rec').read();m=re.search(r'sandbox_id = (.+)',r);print(json.loads(eval(m.group(1)))['workspace'])")"
events="$(python3 -c "import json,re;r=open('$rec').read();m=re.search(r'sandbox_id = (.+)',r);print(json.loads(eval(m.group(1)))['creds']+'/.pillbox-opencode-events.sse')")"
"$PILLBOX" session send "$sid" "$TASK" >/dev/null 2>&1
for _ in $(seq 1 "$((MAX_WAIT / 2))"); do sleep 2; grep -aq 'session.idle' "$events" 2>/dev/null && break; done

if [ -s "$clone/PROFILE.md" ]; then
  cp "$clone/PROFILE.md" "$out"
  echo "propose: wrote $(wc -l < "$out") lines → $out"
else
  echo "propose: agent left PROFILE.md empty" >&2; "$PILLBOX" session rm "$sid" >/dev/null 2>&1; exit 1
fi
"$PILLBOX" session rm "$sid" >/dev/null 2>&1 || true
