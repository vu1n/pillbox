#!/usr/bin/env bash
# σ̂-segmentation experiment harness (GHOST-006) — the keystone measurement of
# the whole variance frame (docs/optimization-gate.md): does cutting a long-
# horizon coding task into rubric-gated SEGMENTS reduce the trial-to-trial
# variance σ̂ that best-of-k otherwise has to pay to overcome?
#
# Two arms per task, TRIALS each, scored on the SAME authoritative full rubric so
# the scores are comparable:
#   monolithic  one session does the whole task in one long horizon.
#   segmented   a FRESH session per segment over the prior segment's VERIFIED
#               workspace — the horizon is RESET at every checkpoint (the faithful
#               operationalization of the hypothesis: in-session chaining would
#               let context accumulate and NOT reset the horizon). Each segment is
#               gated by its authoritative sub-rubric (retried up to SEG_RETRIES);
#               the gate only steers progression — the comparable score is the
#               full rubric at the end, which bounds the weak-verifier confound.
#
# Relationship to `pillbox dispatch` (GHOST-003/004): this is dispatch's
# SEGMENTATION sibling — same run→drive→score→pull primitives (GHOST-004
# de-risked them on libkrun), but it CHAINS short horizons where dispatch FORKS
# one horizon k ways. They compose: SEG_K>1 would run best-of-k per segment (the
# dispatch lever) on top of segmentation — a follow-up, not the keystone, so the
# default SEG_K=1 isolates segmentation as the single variable under test.
#
# Two confounders this rig must control (arXiv 2603.29231, the research framing):
#   1. verifier quality — a weak gate adds latency without cutting variance, so a
#      null could be weak-gate not weak-segmentation. We pin the gates to the
#      task's OWN hidden tests (authoritative subsets), not hand-written checks.
#   2. capability headroom — a saturated task family (toolz) has no capability-
#      variance to express, so segmentation can't show a delta. ap_pov is chosen
#      precisely because its long horizon makes the monolithic arm bistable (high
#      σ̂) — the room a cut needs to land in.
# A third, mechanical one: TRUNCATION. The monolithic arm's identity is a long
# horizon; too small a MAX_WAIT cuts it off and inflates its failure (the prior
# σ̂ finding: 600s partials were truncation artifacts). MAX_WAIT defaults generous.
#
# Usage: run.sh [--dry-run] [--trials N] [task-ref...]
#   task-ref : a task dir (prompt.txt + workspace/ + grader/[rubric.txt]) or a
#              frozen bookmark <set>/<split>/<id> in the evals pillbox. Each task
#              MUST have a segment decomposition under segments/<basename>/NN-*/
#              {prompt.txt,rubric.txt}. Default (no refs): every task that has a
#              committed segment spec, resolved to ../tasks/<name>.
#   --dry-run: print the trial matrix (both arms × trials, task ids, segment
#              specs, rubric paths resolved) WITHOUT launching anything. The gate.
#
# Env: PILLBOX (binary), MODEL (provider/modelID), TRIALS (default 10),
#      SEG_RETRIES (per-segment gate retries, default 1; SET TO 0 for the H2
#      isolation arm — pure horizon-reset, no retry), TEMPERATURE (default 0,
#      greedy — the variance knob), MAX_WAIT (per-turn idle cap, default 600),
#      LAUNCH_RETRIES (transient-launch retry budget, default 3),
#      EVALS_PILLBOX (frozen-task store, default `evals`), PRICE_*_PER_M (cost),
#      OUT (JSONL records path; default a tempfile, printed at the end).
#
# Substrate note: `session rm` does NOT clean per-session krun state
# (~/.pillbox/krun/{creds,ws}/* + *.sock) — the accumulation degrades fresh-VM
# launches over a long batch (the H1 mid-run stall). The harness compensates:
# `reap_session` removes each session's state on teardown. The proper fix belongs
# in pillbox's own session-rm; until then this keeps a multi-hour campaign healthy.
#
# Live runs need a codesigned libkrun binary (scripts/lk-build.sh), opencode
# authed, the runner image present, and the task materialized (ap_pov via
# scripts/eval/import-aider-polyglot.py). GHOST-007 executes it and records the
# verdict; this task's gate is `--dry-run` green.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../../target/debug/pillbox}"
TRIALS="${TRIALS:-10}"
SEG_RETRIES="${SEG_RETRIES:-1}"
MAX_WAIT="${MAX_WAIT:-600}"
# Bounded retry for a transient libkrun launch failure — a fresh VM can
# intermittently fail to boot under sustained churn (the krun-state leak below
# degrades launches over a long batch). Without this a multi-task run silently
# drops trials to the transient. 0 disables.
LAUNCH_RETRIES="${LAUNCH_RETRIES:-3}"
# Hard caps (macOS has no `timeout`, so the helpers below use a background+kill
# watchdog). The H1 hang was an unbounded `session send` to a half-dead VM
# blocking forever; LAUNCH_TIMEOUT bounds a half-booting VM the same way.
SEND_TIMEOUT="${SEND_TIMEOUT:-60}"
LAUNCH_TIMEOUT="${LAUNCH_TIMEOUT:-90}"
EVALS_PILLBOX="${EVALS_PILLBOX:-evals}"
export PILLBOX_BACKEND="${PILLBOX_BACKEND:-libkrun}"
export TEMPERATURE="${TEMPERATURE:-0}" # greedy by default — isolate segmentation
# shellcheck source=../lib.sh
. "$here/../lib.sh"  # pb_run_session / pb_workspace / pb_drive_and_wait / pb_score_* / pb_usage

# ── argument parsing ─────────────────────────────────────────────────────────
DRY=0
REFS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift;;
    --trials|-n) TRIALS="$2"; shift 2;;
    -h|--help) sed -n '2,46p' "$0"; exit 0;;
    -*) echo "unknown flag: $1" >&2; exit 2;;
    *) REFS+=("$1"); shift;;
  esac
done
case "$TRIALS" in ''|*[!0-9]*) echo "--trials must be a positive integer" >&2; exit 2;; esac

# Default task set: every committed segment registry entry → its ../tasks/<name>.
if [ "${#REFS[@]}" -eq 0 ]; then
  for d in "$here"/segments/*/; do
    [ -d "$d" ] || continue
    REFS+=("$here/../tasks/$(basename "$d")")
  done
fi
[ "${#REFS[@]}" -ge 1 ] || { echo "no tasks: no segment specs under $here/segments/" >&2; exit 2; }

seg_root_for() { printf '%s' "$here/segments/$1"; }

# ── dry-run: print the trial matrix, resolve every path, never launch ─────────
print_matrix() {
  echo "σ̂-segmentation experiment — trial matrix (dry-run, no sessions launched)"
  echo "  trials/arm: $TRIALS   arms: monolithic, segmented   tasks: ${#REFS[@]}"
  echo "  model: ${MODEL:-<opencode default>}   temperature: $TEMPERATURE   seg-retries: $SEG_RETRIES   backend: $PILLBOX_BACKEND"
  echo
  local ref name segd cells=0 boots=0
  for ref in "${REFS[@]}"; do
    name="$(basename "$ref")"
    segd="$(seg_root_for "$name")"
    echo "task: $name   (source: $ref)"

    # The full rubric both arms are graded on. Present when the task dir is
    # materialized (a dir ref, or ../tasks/<name> after the importer ran).
    if [ -f "$ref/grader/rubric.txt" ]; then
      echo "  full rubric (both arms): $ref/grader/rubric.txt  [resolved]"
    elif [ -f "$ref/grader/grade.sh" ]; then
      echo "  full rubric (both arms): $ref/grader/grade.sh  [resolved, binary grade]"
    else
      echo "  full rubric (both arms): $ref/grader/rubric.txt  [materialize the task before a live run]"
    fi
    echo "  arm monolithic × $TRIALS trials   prompt: $ref/prompt.txt"

    if [ ! -d "$segd" ]; then
      echo "  arm segmented: ERROR — no segment spec at segments/$name/ (cannot segment this task)" >&2
      DRY_ERR=1
      echo
      continue
    fi
    echo "  arm segmented  × $TRIALS trials   segments (fresh session each, chained):"
    local d i=0 sp sr spm srm
    for d in "$segd"/*/; do
      [ -d "$d" ] || continue
      i=$((i + 1))
      sp="${d}prompt.txt"; sr="${d}rubric.txt"
      if [ -f "$sp" ]; then spm=resolved; else spm=MISSING; DRY_ERR=1; fi
      if [ -f "$sr" ]; then srm=resolved; else srm=MISSING; DRY_ERR=1; fi
      printf '    %d. %-14s prompt: %s [%s]   rubric: %s [%s]\n' \
        "$i" "$(basename "$d")" "$sp" "$spm" "$sr" "$srm"
    done
    if [ "$i" -eq 0 ]; then
      echo "    ERROR — segments/$name/ has no NN-*/ segment dirs" >&2
      DRY_ERR=1
    fi
    # monolithic: 1 VM/trial; segmented: i VMs/trial (a fresh session per segment).
    boots=$((boots + TRIALS + i * TRIALS))
    cells=$((cells + 2 * TRIALS))
    echo
  done
  echo "totals: $cells cells (${#REFS[@]} tasks × 2 arms × $TRIALS trials) → ~$boots VM boots"
  echo "stats:  paired-stats.py (paired lift CI + pooled σ̂) + a per-arm σ̂ summary"
}

if [ "$DRY" = 1 ]; then
  DRY_ERR=0
  print_matrix
  [ "$DRY_ERR" = 0 ] || { echo "dry-run: a segment spec/rubric did not resolve (see above)" >&2; exit 1; }
  exit 0
fi

# ── live cells (executed by GHOST-007; gate here is --dry-run) ────────────────
out="${OUT:-$(mktemp)}"
: >"$out"

# Append one JSONL trial record (the paired-stats input schema).
emit_record() { # task cond trial score cost
  python3 -c 'import json,sys
print(json.dumps({"task":sys.argv[1],"cond":sys.argv[2],"trial":int(sys.argv[3]),
                  "score":float(sys.argv[4] or 0),"cost":float(sys.argv[5] or 0)}))' \
    "$1" "$2" "$3" "${4:-0}" "${5:-0}" >>"$out"
}

# The full rubric grade flags for a task (rubric.txt preferred; grade.sh binary
# fallback) — single-sourced so both arms grade identically.
full_grade_flags() { # task_dir → echoes the score flags
  if [ -f "$1/grader/rubric.txt" ]; then
    printf -- '--rubric\n%s\n' "$1/grader/rubric.txt"
  else
    printf -- '--cmd\nsh grade.sh\n'
  fi
}

# Pull costUsd from a pb_usage JSON line. Empty/unparseable → 0: a session may
# log no usage events, and cost is best-effort (off the σ̂ metric), so 0 is the
# correct sentinel here, not a swallowed error.
cost_of() { python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("costUsd",0))
except Exception: print(0)'; }
add_floats() { python3 -c 'import sys;print(float(sys.argv[1])+float(sys.argv[2]))' "$1" "$2"; }

# Launch a worker session over workspace $1. BOUNDS the launch (LAUNCH_TIMEOUT)
# via a background+kill watchdog — a half-booting VM can hang `run --json` (the
# python parse blocks on a pipe that never EOFs), and an unbounded launch would
# freeze the whole batch. Retries a transient failure (empty sid) up to
# LAUNCH_RETRIES with linear backoff. Echoes the sid, or empty after the budget —
# the caller records that trial as a 0, so a persistent failure degrades gracefully.
launch_session() {
  local sid attempt tmpf lp w
  for attempt in $(seq 0 "$LAUNCH_RETRIES"); do
    tmpf="$(mktemp)"
    ( pb_run_session "$1" >"$tmpf" 2>/dev/null ) &
    lp=$!
    w=0
    while kill -0 "$lp" 2>/dev/null && [ "$w" -lt "$LAUNCH_TIMEOUT" ]; do sleep 3; w=$((w + 3)); done
    kill -0 "$lp" 2>/dev/null && kill -9 "$lp" 2>/dev/null
    wait "$lp" 2>/dev/null
    sid="$(cat "$tmpf" 2>/dev/null)"
    rm -f "$tmpf"
    [ -n "$sid" ] && { printf '%s' "$sid"; return 0; }
    [ "$attempt" -lt "$LAUNCH_RETRIES" ] && sleep "$(((attempt + 1) * 5))"
  done
  return 1
}

# Tear down session $1 AND reap its leaked krun state (creds/workspace/sock):
# `session rm` kills the VM but leaves these on disk, and the accumulation is what
# degrades fresh-VM launches across a long batch (the GHOST-007 scaling note → the
# H1 mid-run stall). Capture the sandbox paths from `session info` before rm, then
# remove them. Safe: callers copy the clone out (grade / segment handoff) first.
reap_session() { # sid
  [ -n "$1" ] || return 0
  local info; info="$("$PILLBOX" session info "$1" --json 2>/dev/null)"
  "$PILLBOX" session rm "$1" >/dev/null 2>&1
  printf '%s' "$info" | python3 -c '
import json, sys, os, shutil
try: sb = json.loads(json.load(sys.stdin)["session"]["sandbox_id"])
except Exception: sys.exit(0)
for k in ("creds", "workspace", "sock"):
    p = sb.get(k)
    if not isinstance(p, str) or not p: continue
    try:
        shutil.rmtree(p, ignore_errors=True) if os.path.isdir(p) else os.remove(p)
    except Exception: pass
' 2>/dev/null
}

# Bounded drive: `session send` has no timeout, so a half-dead VM blocks the batch
# forever (the H1 hang). Cap the send with a background+kill watchdog; wait-idle is
# already bounded by MAX_WAIT. A send that overruns → the turn is abandoned and the
# cell grades whatever landed.
drive_bounded() { # sid prompt
  local sp w=0
  ( "$PILLBOX" session send "$1" "$2" >/dev/null 2>&1 ) &
  sp=$!
  while kill -0 "$sp" 2>/dev/null && [ "$w" -lt "$SEND_TIMEOUT" ]; do sleep 2; w=$((w + 2)); done
  kill -0 "$sp" 2>/dev/null && kill -9 "$sp" 2>/dev/null
  wait "$sp" 2>/dev/null
  "$PILLBOX" session wait-idle "$1" --timeout "$MAX_WAIT" >/dev/null 2>&1 || true
}

# Grade session $1's clone $2 against task $3's FULL rubric, in a throwaway copy
# with the hidden grader injected (invisible to any later turn) → echoes the
# fractional score. The authoritative, comparable metric for both arms.
grade_full() { # sid clone task_dir
  local scoredir; scoredir="$(mktemp -d)"
  cp -R "$2/." "$scoredir"/ 2>/dev/null
  cp -R "$3/grader/." "$scoredir"/ 2>/dev/null
  local flags=(); local line
  while IFS= read -r line; do flags+=("$line"); done < <(full_grade_flags "$3")
  local sj; sj="$("$PILLBOX" session score "$1" "${flags[@]}" --workspace "$scoredir" --json 2>/dev/null || true)"
  rm -rf "$scoredir"
  printf '%s' "$sj" | pb_score_value
}

# Arm A — one session, the whole task in one horizon.
run_monolithic_cell() { # task_dir task trial
  local ws; ws="$(mktemp -d)"
  cp -R "$1/workspace/." "$ws"/ 2>/dev/null
  local sid; sid="$(launch_session "$ws")"
  if [ -z "$sid" ]; then rm -rf "$ws"; emit_record "$2" monolithic "$3" 0 0; return; fi
  local clone; clone="$(pb_workspace "$sid")"
  if [ -z "$clone" ]; then reap_session "$sid"; rm -rf "$ws"; emit_record "$2" monolithic "$3" 0 0; return; fi
  drive_bounded "$sid" "$(cat "$1/prompt.txt")"
  local score; score="$(grade_full "$sid" "$clone" "$1")"
  local cost; cost="$(pb_usage "$sid" | cost_of)"
  reap_session "$sid"
  rm -rf "$ws"
  emit_record "$2" monolithic "$3" "$score" "$cost"
}

# Drive + gate one segment (best-effort, SEG_RETRIES): the gate only steers
# progression; the final full-rubric grade is the metric. The gate runs the
# segment's authoritative sub-rubric in a throwaway copy with the hidden grader
# injected, so neither the checks nor their artifacts leak into the next segment.
gate_segment() { # sid clone segdir task_dir
  local prompt; prompt="$(cat "$3/prompt.txt")"
  local scoredir sj state
  for _ in $(seq 0 "$SEG_RETRIES"); do
    drive_bounded "$1" "$prompt"
    scoredir="$(mktemp -d)"
    cp -R "$2/." "$scoredir"/ 2>/dev/null
    cp -R "$4/grader/." "$scoredir"/ 2>/dev/null
    sj="$("$PILLBOX" session score "$1" --rubric "$3/rubric.txt" --workspace "$scoredir" --json 2>/dev/null || true)"
    rm -rf "$scoredir"
    state="$(printf '%s' "$sj" | pb_score_state)"
    [ "$state" = satisfied ] && return 0
    prompt="$(printf '%s' "$sj" | pb_failed_feedback)"
  done
  return 0
}

# Arm B — a fresh session per segment over the prior segment's verified
# workspace; the horizon resets at each checkpoint. Final tree graded on the same
# full rubric as arm A.
run_segmented_cell() { # task_dir task trial
  local segd; segd="$(seg_root_for "$2")"
  local segs=() d
  for d in "$segd"/*/; do [ -d "$d" ] && segs+=("$d"); done
  local nseg=${#segs[@]}
  if [ "$nseg" -eq 0 ]; then emit_record "$2" segmented "$3" 0 0; return; fi

  local ws_prev; ws_prev="$(mktemp -d)"
  cp -R "$1/workspace/." "$ws_prev"/ 2>/dev/null
  local cost=0 sid="" clone="" i=0 segcost ws_next
  for d in "${segs[@]}"; do
    i=$((i + 1))
    sid="$(launch_session "$ws_prev")"
    if [ -z "$sid" ]; then rm -rf "$ws_prev"; emit_record "$2" segmented "$3" 0 "$cost"; return; fi
    clone="$(pb_workspace "$sid")"
    if [ -z "$clone" ]; then reap_session "$sid"; rm -rf "$ws_prev"; emit_record "$2" segmented "$3" 0 "$cost"; return; fi
    gate_segment "$sid" "$clone" "$d" "$1"
    segcost="$(pb_usage "$sid" | cost_of)"
    cost="$(add_floats "$cost" "$segcost")"
    if [ "$i" -lt "$nseg" ]; then
      # Hand the VERIFIED clone forward as the next checkpoint (the grader lived in
      # a throwaway, so the clone is clean), then reset the horizon: a new session.
      ws_next="$(mktemp -d)"
      cp -R "$clone/." "$ws_next"/ 2>/dev/null
      reap_session "$sid"
      rm -rf "$ws_prev"; ws_prev="$ws_next"
    fi
  done
  # Final authoritative grade on the last segment's clone (its session still live).
  local score; score="$(grade_full "$sid" "$clone" "$1")"
  reap_session "$sid"
  rm -rf "$ws_prev"
  emit_record "$2" segmented "$3" "$score" "$cost"
}

# Resolve a task ref to a local dir (a dir in place; a frozen bookmark pulled).
resolve_task_ref() {
  if [ -d "$1" ]; then printf '%s' "$1"; return 0; fi
  local td; td="$(mktemp -d)"
  if ( cd "$td" && "$PILLBOX" --pillbox "$EVALS_PILLBOX" pull --bookmark "$1" ) >/dev/null 2>&1; then
    printf '%s' "$td"; return 0
  fi
  rm -rf "$td"; return 1
}

for ref in "${REFS[@]}"; do
  name="$(basename "$ref")"
  if [ ! -d "$(seg_root_for "$name")" ]; then echo "skip (no segment spec): $name" >&2; continue; fi
  if ! task_dir="$(resolve_task_ref "$ref")"; then echo "skip (no task dir / frozen bookmark): $ref" >&2; continue; fi
  for t in $(seq 1 "$TRIALS"); do
    echo "▶ $name trial $t/$TRIALS" >&2
    run_monolithic_cell "$task_dir" "$name" "$t"
    run_segmented_cell  "$task_dir" "$name" "$t"
  done
  # rm only a tempdir we pulled; a dir ref ($ref == $task_dir) is used in place.
  [ "$ref" = "$task_dir" ] || rm -rf "$task_dir"
done

echo "=== records: $out ===" >&2
echo "=== paired-stats (monolithic=A, segmented=B) ===" >&2
python3 "$here/../paired-stats.py" --baseline monolithic --treatment segmented "$out" || true
echo "=== per-arm σ̂ — the keystone: does segmentation cut it? ===" >&2
python3 - "$out" <<'PY'
import json, sys, statistics, collections
cells = collections.defaultdict(list)
passes = collections.defaultdict(lambda: [0, 0])
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    r = json.loads(line)
    cells[(r["task"], r["cond"])].append(r["score"])
    passes[r["cond"]][0] += 1 if r["score"] >= 0.999 else 0
    passes[r["cond"]][1] += 1
per = collections.defaultdict(list)
for (task, cond), v in cells.items():
    if len(v) >= 2:
        per[cond].append(statistics.stdev(v))
for cond in sorted(per):
    sds = per[cond]
    sig = sum(sds) / len(sds) if sds else 0.0
    p, n = passes[cond]
    print(f"  {cond:11s} sigma_hat={sig:.4f}  pass-rate={p}/{n}={(p/n if n else 0):.2f}")
ms, ss = per.get("monolithic"), per.get("segmented")
if ms and ss:
    a, b = sum(ms) / len(ms), sum(ss) / len(ss)
    print(f"  delta_sigma = segmented - monolithic = {b - a:+.4f}  ({'cuts' if b < a else 'raises'} variance)")
PY
