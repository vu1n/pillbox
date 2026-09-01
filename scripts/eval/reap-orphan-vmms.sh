#!/usr/bin/env bash
# DEPRECATED — both leak paths below are now fixed at the SOURCE in pillbox; this
# host-side daemon is a TRANSITIONAL net only (for orphans left by an OLD binary,
# or pre-fix strays). With a current `pillbox` it should find nothing to reap.
#   - Path 1 (wedged/argv-drift VMM survives `session rm`): fixed — `session rm`
#     now reaps by SPEC PATH (`reap_vmm_by_spec`), drift-proof past the reparent.
#   - Path 2 (launcher `kill -9`'d before the record commits → orphan-no-record):
#     fixed — a detached VMM arms a self-destruct COMMIT GUARD and tears its own VM
#     down if its launcher dies before the session record appears. The VMM cleans up
#     after itself, so the host no longer reaps across independent pillboxes.
# Retire this once campaigns run the fixed binary everywhere.
#
# Periodic orphan-`__krun-vmm` reaper for DEDICATED libkrun eval runs.
#
# Why this existed: a clean `session rm` reaps its VMM correctly, but under
# concurrent churn two leak paths survived it —
#   1. a wedged/half-launched HVF VMM whose `ps` argv no longer matches its spec
#      path → `kill_vmm_group`'s attribution check fails → it takes the "leave it"
#      branch (warning suppressed by the rig's 2>&1) and the record is deleted
#      anyway → orphan-no-record;
#   2. `launch_session`'s watchdog `kill -9`s a slow `run` AFTER it spawned the
#      detached (setsid) VMM but BEFORE it recorded a session → orphan with no
#      record to reap.
# Both accumulate over a long batch (see pillbox-libkrun-host-fragility mode 3/4)
# and degrade later launches.
#
# Strategy: on a DEDICATED host (only this campaign uses libkrun) any `__krun-vmm`
# whose pid is NOT a live recorded session's VMM pid is an orphan. Kill its process
# GROUP (the VMM is its own group leader via setsid; pid==pgid) — `killpg` reaps
# the wedged leader + any forked VMM subprocess that a pid-only SIGKILL strands.
#
# SAFETY: a 2-consecutive-rounds grace gate means a VMM that's merely mid-launch
# (not yet recorded — a <~10s window) is never killed; only a pid that's been
# orphaned across two scans (~1-2×INTERVAL) is reaped. DO NOT run this when other
# pillbox work shares the host — it can't tell another pillbox's live VM from a
# local orphan (the multi-pillbox attribution wall).
#
#   Usage: reap-orphan-vmms.sh        (runs until killed; log to stdout)
# Env: PILLBOX (binary), INTERVAL (scan seconds, default 60).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
INTERVAL="${INTERVAL:-60}"

# Space-padded list of pids backing a live recorded session (the protected set).
live_pids() {
  local sid sb
  for sid in $("$PILLBOX" session list 2>/dev/null | awk '/running/{print $1}'); do
    sb="$("$PILLBOX" session info "$sid" --json 2>/dev/null \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["session"]["sandbox_id"])' 2>/dev/null)"
    printf '%s' "$sb" | python3 -c 'import json,sys;print(json.load(sys.stdin)["pid"])' 2>/dev/null
  done
}

echo "$(date '+%H:%M:%S') reaper up (interval ${INTERVAL}s, 2-round grace) — pillbox=$PILLBOX"
prev=" "   # orphan pids seen LAST round; kill only those still orphaned THIS round
while :; do
  live=" $(live_pids | tr '\n' ' ') "
  cur=" "
  for pid in $(pgrep -f '__krun-vmm' 2>/dev/null); do
    case "$live" in *" $pid "*) continue;; esac          # backs a live session → protected
    cur="$cur$pid "                                       # candidate orphan this round
    case "$prev" in
      *" $pid "*)                                         # also orphan last round → reap
        pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')"
        [ -n "$pgid" ] || continue
        echo "$(date '+%H:%M:%S') reaping orphan __krun-vmm pid=$pid pgid=$pgid"
        kill -- -"$pgid" 2>/dev/null
        sleep 1
        kill -0 "$pid" 2>/dev/null && kill -9 -- -"$pgid" 2>/dev/null
        ;;
    esac
  done
  prev="$cur"
  sleep "$INTERVAL"
done
