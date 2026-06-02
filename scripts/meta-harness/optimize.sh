#!/usr/bin/env bash
# Meta-harness v0 — a GEPA-lite instruction-profile optimizer, all through pillbox.
#
#   round r: eval the current best profile on the TRAIN tasks (capturing failures)
#            → propose.sh reflects on those failures → a candidate profile
#            → eval the candidate on a disjoint HELD-OUT set
#            → keep it iff it beats the best held-out score.
#
# Held-out measurement is the point: the profile is distilled from TRAIN failures
# but scored on tasks it never saw, so a gain is GENERALIZATION, not
# teaching-to-the-test. Output: the best profile + the per-round held-out scores.
#
# Usage: optimize.sh [--rounds R] [--out FILE]
# Env: PILLBOX_RUNNER_IMAGE, MODEL (use a capable model), MAX_WAIT.
#
# COST: each round ≈ (|train| evals) + 1 propose + (|heldout| evals) agent
# sessions. Size the task pool + rounds to your budget.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
eval_dir="$here/../eval"
rounds=2 out="$here/best-profile.md"
while [ $# -gt 0 ]; do case "$1" in
  --rounds) rounds="$2"; shift 2;;
  --out) out="$2"; shift 2;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done

# Split the task pool: even-indexed → TRAIN (distill from), odd → HELD-OUT (score on).
mapfile -t pool < <(ls -d "$eval_dir"/tasks/ap_*/ 2>/dev/null | sort)
[ "${#pool[@]}" -ge 2 ] || { echo "need >=2 ap_ tasks; run import-aider-polyglot.py" >&2; exit 1; }
train=() held=()
for i in "${!pool[@]}"; do [ $((i % 2)) -eq 0 ] && train+=("${pool[$i]}") || held+=("${pool[$i]}"); done
echo "train=${#train[@]} held=${#held[@]}"

# eval_set <profile|baseline> <faildir|""> <task...> → echoes "passes/total"
# Runs TRIALS attempts per task (the model is stochastic — single-trial verdicts
# wobble run-to-run, so `select` would otherwise pick on noise). Set TRIALS>=3
# for a stable held-out score.
eval_set() {
  local prof="$1" fd="$2"; shift 2
  local trials="${TRIALS:-1}" p=0 n=0
  for t in "$@"; do
    for _ in $(seq 1 "$trials"); do
      pkill -f __krun-vmm 2>/dev/null; sleep 1
      local v
      v="$(FAILDIR="$fd" bash "$eval_dir/run-task.sh" "$t" "$prof" 2>/dev/null | awk -F'\t' 'END{print $3}')"
      n=$((n + 1)); [ "$v" = pass ] && p=$((p + 1))
    done
  done
  echo "$p/$n"
}

best_profile="baseline"
held_base="$(eval_set baseline "" "${held[@]}")"
echo "round 0 (baseline) held-out: $held_base"
best_score="${held_base%%/*}"

for r in $(seq 1 "$rounds"); do
  fd="$(mktemp -d)"
  train_score="$(eval_set "$best_profile" "$fd" "${train[@]}")"
  if [ -z "$(ls -A "$fd" 2>/dev/null)" ]; then
    echo "round $r: no train failures to learn from (train $train_score) — stop"; break
  fi
  cand="$(mktemp).md"
  prof_arg=(); [ "$best_profile" != baseline ] && prof_arg=(--profile "$best_profile")
  bash "$here/propose.sh" --faildir "$fd" --out "$cand" "${prof_arg[@]}"
  cand_held="$(eval_set "$cand" "" "${held[@]}")"
  echo "round $r: train $train_score → candidate held-out $cand_held (best $best_score/${#held[@]})"
  if [ "${cand_held%%/*}" -gt "$best_score" ]; then
    best_score="${cand_held%%/*}"; best_profile="$cand"; cp "$cand" "$out"
    echo "round $r: KEPT (new best held-out $cand_held) → $out"
  fi
done

echo "=== best held-out $best_score/${#held[@]}; profile: ${best_profile} ==="
[ "$best_profile" != baseline ] && cp "$best_profile" "$out" && echo "wrote $out"
