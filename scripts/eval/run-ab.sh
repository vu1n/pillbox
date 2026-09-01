#!/usr/bin/env bash
# A/B the run-time memory loop: for every task under tasks/, run BOTH conditions
# (baseline vs memory) `TRIALS` times each, grade with `session score`, and
# tabulate pass rates. This is the "prove it before you build it" gate
# (swarm-memory.md): if `memory` doesn't beat `baseline`, neither the run-time
# (ACE) nor the compile-time (GEPA) loop is worth building.
#
# Usage: run-ab.sh [TRIALS]   (default 3)
# Env: CONDITIONS (space-separated arms, default "baseline memory"; e.g.
#      "baseline kypp-briefing" for the kypp memory rung), KYPP_PROJECT,
#      PILLBOX, PILLBOX_RUNNER_IMAGE, MAX_WAIT (passed through to run-task.sh).
#
# NOTE: each trial is a real agent session (model calls + a microVM boot). N
# tasks × 2 conditions × TRIALS sessions — size it to your model budget. Signal
# only emerges on tasks where a playbook bullet addresses a gotcha the baseline
# actually trips on; the bundled `add` task is a trivial plumbing smoke.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
trials="${1:-3}"
results="$(mktemp)"

for task_dir in "$here"/tasks/*/; do
  for cond in ${CONDITIONS:-baseline memory}; do
    for _ in $(seq 1 "$trials"); do
      bash "$here/run-task.sh" "$task_dir" "$cond" >>"$results"
    done
  done
done

echo "=== pass rate (task / condition: passes/total) ==="
awk -F'\t' '{tot[$1"\t"$2]++; if($3=="pass") pass[$1"\t"$2]++}
  END{for(k in tot) printf "%s\t%d/%d\n", k, pass[k]+0, tot[k]}' "$results" | sort
rm -f "$results"
