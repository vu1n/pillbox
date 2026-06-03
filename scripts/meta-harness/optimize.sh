#!/usr/bin/env bash
# The gate: a 3-arm bakeoff on a FROZEN held-out split, all through pillbox.
# Decides whether the GEPA-style optimizer earns its keep over the cheaper arms
# BEFORE committing to a DSPy/GEPA meta-harness (docs/swarm-memory.md verdict).
#
# GEPA arm — round r: eval the current best profile on the TRAIN tasks (capturing
#   failures) → propose.sh reflects on those failures → a candidate profile →
#   eval the candidate on the HELD-OUT set → keep it iff it beats the best.
#   Held-out is the point: distilled from TRAIN failures, scored on unseen tasks,
#   so a gain is GENERALIZATION not teaching-to-the-test.
# Compared against two cheaper arms on the SAME held-out split:
#   baseline — strong base model, no profile.
#   ACE      — the curated memory/playbook prepended at run time.
#
# Usage: optimize.sh [--rounds R] [--out FILE]
# Env: MODEL (capable model), MAX_WAIT, TRIALS (>=3 — the model is stochastic;
#      single-trial verdicts wobble), SET (frozen set, default aider),
#      EVALS_PILLBOX (store, default evals), PILLBOX_RUNNER_IMAGE.
#
# COST: (|train|+|heldout|)·TRIALS·rounds + |heldout|·TRIALS (ACE) agent sessions.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
eval_dir="$here/../eval"
rounds=2 out="$here/best-profile.md"
while [ $# -gt 0 ]; do case "$1" in
  --rounds) rounds="$2"; shift 2;;
  --out) out="$2"; shift 2;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done

# The pool is the FROZEN eval set in the `evals` pillbox (freeze-task.sh): refs
# SET/train/* (distill from) and SET/held-out/* (scored on, never trained on).
# run-task.sh pulls each ref back to an identical tree per attempt, so a round's
# score is reproducible. SET selects the frozen set; EVALS_PILLBOX the store.
PILLBOX="${PILLBOX:-$here/../../target/debug/pillbox}"
SET="${SET:-aider}"
EVALS_PILLBOX="${EVALS_PILLBOX:-evals}"
frozen_refs() { "$PILLBOX" --pillbox "$EVALS_PILLBOX" bookmark list --json 2>/dev/null \
  | python3 -c 'import json,sys;[print(b["name"]) for b in json.load(sys.stdin)["bookmarks"]]'; }
train=() held=()  # read-loop, not mapfile — macOS ships bash 3.2
while IFS= read -r r; do train+=("$r"); done < <(frozen_refs | grep "^$SET/train/" | sort)
while IFS= read -r r; do held+=("$r");  done < <(frozen_refs | grep "^$SET/held-out/" | sort)
{ [ "${#train[@]}" -ge 1 ] && [ "${#held[@]}" -ge 1 ]; } || {
  echo "need frozen $SET/{train,held-out}/* in the '$EVALS_PILLBOX' pillbox; run freeze-task.sh" >&2; exit 1; }
echo "frozen set '$SET': train=${#train[@]} held=${#held[@]}"

# eval_set <profile|baseline> <faildir|""> <task...> → echoes "passes/total"
# Runs TRIALS attempts per task (the model is stochastic — single-trial verdicts
# wobble run-to-run, so `select` would otherwise pick on noise). Set TRIALS>=3
# for a stable held-out score.
#
# Each attempt is hard-capped at CAP seconds: a single wedged turn must not stall
# the whole batch (one task once hung ~40min while the per-task path runs in ~90s).
# On expiry the attempt is killed and counts as a non-pass. After every attempt we
# reap stray VMs + opencode servers (a killed task skips its own `session rm`), so
# each task starts from a clean slate — the runs are strictly serial.
CAP=$(( ${MAX_WAIT:-120} + 150 ))
reap() { pkill -f __krun-vmm 2>/dev/null; pkill -f 'pillbox run --agent opencode' 2>/dev/null; }
# Echoes "<perfect>/<n> <mean>": pass-count (tasks scoring 1.0) AND the mean
# fractional rubric score — the low-noise gate metric (a partial 18/20-tests-pass
# shows as 0.9, not a flat fail, so arms separate without many trials). A capped/
# errored attempt scores 0.
eval_set() {
  local prof="$1" fd="$2"; shift 2
  local trials="${TRIALS:-1}" p=0 n=0 scores=""
  for t in "$@"; do
    for _ in $(seq 1 "$trials"); do
      local out rp wp v sv; out="$(mktemp)"
      FAILDIR="$fd" bash "$eval_dir/run-task.sh" "$t" "$prof" >"$out" 2>/dev/null & rp=$!
      ( sleep "$CAP"; kill -TERM "$rp" 2>/dev/null; sleep 3; kill -KILL "$rp" 2>/dev/null ) & wp=$!
      # a capped (killed) task makes `wait` return nonzero — tolerate it under set -e.
      wait "$rp" 2>/dev/null || true; kill "$wp" 2>/dev/null || true; wait "$wp" 2>/dev/null || true
      reap; sleep 1
      v="$(awk -F'\t' 'END{print $3}' "$out")"
      sv="$(awk -F'\t' 'END{print ($4==""?0:$4)}' "$out")"; rm -f "$out"
      n=$((n + 1)); [ "$v" = pass ] && p=$((p + 1)); scores="$scores $sv"
      echo "  $(basename "$t") [$( [ "$prof" = baseline ] && echo base || echo "$(basename "$prof")")] → ${v:-timeout} score=${sv:-0}" >&2
    done
  done
  echo "$p/$n $(echo "$scores" | awk '{s=0;for(i=1;i<=NF;i++)s+=$i; printf "%.3f", (NF?s/NF:0)}')"
}

best_profile="baseline"
held_base="$(eval_set baseline "" "${held[@]}")"
echo "round 0 (baseline) held-out: $held_base"
best_mean="${held_base##* }" best_held="$held_base"

for r in $(seq 1 "$rounds"); do
  fd="$(mktemp -d)"
  train_score="$(eval_set "$best_profile" "$fd" "${train[@]}")"
  if [ -z "$(ls -A "$fd" 2>/dev/null)" ]; then
    echo "round $r: no train failures to learn from (train $train_score) — stop"; break
  fi
  cand="$(mktemp).md"
  # explicit branch, not "${prof_arg[@]}" — an empty array trips set -u on bash 3.2.
  # Guarded: a propose failure skips the GEPA candidate (arm stays at best) rather
  # than aborting the whole bakeoff under set -e.
  pa=(); [ "$best_profile" != baseline ] && pa=(--profile "$best_profile")
  if ! bash "$here/propose.sh" --faildir "$fd" --out "$cand" ${pa[@]+"${pa[@]}"}; then
    echo "round $r: propose failed — keeping best ($best_held)"; continue
  fi
  cand_held="$(eval_set "$cand" "" "${held[@]}")"
  cand_mean="${cand_held##* }"
  echo "round $r: train $train_score → candidate held-out $cand_held (best $best_held)"
  # Keep on the MEAN (float, via awk — bash 3.2 has no float compare), so a graded
  # gain counts even when the perfect-task count is unchanged.
  if awk "BEGIN{exit !($cand_mean > $best_mean)}"; then
    best_mean="$cand_mean"; best_held="$cand_held"; best_profile="$cand"; cp "$cand" "$out"
    echo "round $r: KEPT (new best held-out $cand_held) → $out"
  fi
done
[ "$best_profile" != baseline ] && cp "$best_profile" "$out"

# The third arm: ACE-style runtime context (the curated memory/playbook prepended
# at run time) on the SAME held-out split — the gate compares all three.
held_ace="$(eval_set memory "" "${held[@]}")"

echo
echo "=== 3-arm bakeoff — held-out (n=${#held[@]}, TRIALS=${TRIALS:-1}; metric = mean rubric score, perfect-task count in parens) ==="
echo "  baseline (strong base, no profile) : ${held_base##* }  (${held_base%% *} perfect)"
echo "  ACE      (memory/playbook prepend) : ${held_ace##* }  (${held_ace%% *} perfect)"
echo "  GEPA     (distilled profile)       : ${best_held##* }  (${best_held%% *} perfect)  [${best_profile##*/}]"
echo "verdict: GEPA earns the build iff its mean (${best_held##* }) durably beats baseline (${held_base##* }) AND ACE (${held_ace##* })."
