#!/usr/bin/env bash
# Partition a directory of imported task dirs into train/val/TEST and freeze each
# into the `evals` pillbox as a content-addressed bookmark `<set>/<split>/<id>`
# (via freeze-task.sh). Enforces the eval design's THREE-split discipline
# (docs/optimization-eval-family.md §3): the optimization gate's bug was a
# two-way split where candidates were *selected on* the "held-out" set →
# selection pressure flattered the optimizer. Here:
#   train → distill a playbook/profile from
#   val   → select candidates across rounds
#   test  → LOCKED; scored once, never selected on (the only number that counts)
#
# Assignment is by a STABLE hash of the task id (not order, not shuffle), so a
# re-run reproduces the exact split and adding tasks doesn't reshuffle existing
# ones — a task keeps its split for life (no train↔test leakage across runs).
#
# Usage: freeze-split.sh <tasks-root> <set-name> [train:val:test]
#   tasks-root : dir of task dirs (each with workspace/+grader/+prompt.txt), e.g.
#                a directory produced by import-aider-polyglot.py / import-swebench.py
#   set-name   : the bookmark set (e.g. `aipoly` → bookmarks aipoly/train/<id> …)
#   ratios     : integer weights, default 70:15:15
# Flags (env): DRY_RUN=1 prints the assignment without freezing.
# Env: PILLBOX, EVALS_PILLBOX (default `evals`; one-time: `pillbox new --name evals`).
set -euo pipefail

u="usage: freeze-split.sh <tasks-root> <set-name> [train:val:test]"
root="${1:?$u}"
set_name="${2:?$u}"
ratios="${3:-70:15:15}"
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
export PILLBOX EVALS_PILLBOX="${EVALS_PILLBOX:-evals}"

IFS=: read -r r_train r_val r_test <<<"$ratios"
total=$((r_train + r_val + r_test))
[ "$total" -gt 0 ] || { echo "ratios must sum > 0" >&2; exit 2; }

# Stable split for an id: md5(id) % total → cumulative bucket. The modulo MUST
# happen in python — md5 is a 128-bit int and bash arithmetic is 64-bit, so
# `$((h % total))` on the full hash overflows to garbage. (Ratios are therefore
# approximate for small N — md5 of similar ids clumps; the printed counts are the
# truth. Hash-bucketing, not rank, so a task keeps its split when the set grows.)
split_for() {
  python3 -c '
import sys, hashlib
id, total, rt, rv = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
b = int(hashlib.md5(id.encode()).hexdigest(), 16) % total
print("train" if b < rt else "val" if b < rt + rv else "test")
' "$1" "$total" "$r_train" "$r_val"
}

shopt -s nullglob
count_train=0 count_val=0 count_test=0
for task_dir in "$root"/*/; do
  [ -d "$task_dir/workspace" ] && [ -d "$task_dir/grader" ] || continue
  id="$(basename "$task_dir")"
  split="$(split_for "$id")"
  case "$split" in train) count_train=$((count_train+1));; val) count_val=$((count_val+1));; test) count_test=$((count_test+1));; esac
  if [ "${DRY_RUN:-0}" = 1 ]; then
    printf '%s\t%s\n' "$split" "$id"
  else
    bash "$here/freeze-task.sh" "$task_dir" "$set_name" "$split" "$id" >/dev/null
    printf 'froze %s/%s/%s\n' "$set_name" "$split" "$id"
  fi
done

echo "=== $set_name: train=$count_train val=$count_val test=$count_test (ratios $ratios) ===" >&2
[ $((count_train + count_val + count_test)) -gt 0 ] || { echo "no task dirs under $root" >&2; exit 1; }
