#!/usr/bin/env bash
# H5 — cross-model σ̂. Runs the segmentation experiment (run.sh) once per worker
# MODEL, one OUT file each, then summarizes per-model: monolithic σ̂/mean,
# segmented σ̂/mean, and the paired segmentation lift CI.
#
# The compounding-layer gate: GHOST-007 showed σ̂-reduction-via-segmentation on a
# SINGLE model (zai-coding-plan/glm-5.1). The ultra-review's standing objection is
# "single-model". This sweeps the same experiment across model families to see if
# the variance frame replicates — the prerequisite for building the compounding
# layer (#72 self-harness, learned decomposer).
#
#   Usage: h5-cross-model.sh [model...]   (default: the opencode-go flagship panel)
# Env: TRIALS (default 10), PILLBOX (codesigned libkrun binary), RESULTS_DIR,
#      plus everything run.sh honors (MAX_WAIT, SEG_RETRIES, ENUM_MONO, ...).
#
# Resume: a model whose OUT file already exists and is non-empty is SKIPPED, so a
# stalled sweep re-runs only the unfinished models (run.sh emits JSONL per-trial,
# but this driver's unit of resume is the whole-model file — delete a partial file
# to re-run that model).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
PILLBOX="${PILLBOX:-$here/../../../target/debug/pillbox}"
TRIALS="${TRIALS:-10}"
RESULTS="${RESULTS_DIR:-$here/results}"
mkdir -p "$RESULTS"

# Pin the runner image. run.sh does NOT set one, so an unpinned run falls back to
# the published default (ghcr.io/vu1n/pillbox-runner:latest) which isn't cached
# locally and needs docker → `run` fails instantly → every cell records a SILENT
# 0. l7 is the egress-capable local dev image. (The 2026-06-20 cost==0 footgun.)
export PILLBOX_RUNNER_IMAGE="${PILLBOX_RUNNER_IMAGE:-pillbox-runner:l7}"
export PILLBOX_BACKEND="${PILLBOX_BACKEND:-libkrun}"

# Loud preflight: one real launch must yield a session id, else the whole sweep
# would silently all-0. Fail closed BEFORE booting 100s of VMs for nothing.
preflight() {
  local ws sid; ws="$(mktemp -d)"; : >"$ws/.probe"
  sid="$("$PILLBOX" run --agent opencode --json --workspace "$ws" 2>/dev/null \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["session"]["id"])
except Exception: pass')"
  if [ -n "$sid" ]; then "$PILLBOX" session rm "$sid" >/dev/null 2>&1; rm -rf "$ws"; return 0; fi
  rm -rf "$ws"
  echo "✗ preflight: \`run\` produced no session id — aborting (would all-0)." >&2
  echo "  Check: codesigned libkrun binary, PILLBOX_RUNNER_IMAGE=$PILLBOX_RUNNER_IMAGE present, opencode authed." >&2
  "$PILLBOX" run --agent opencode --json --workspace /tmp 2>&1 | grep -i 'rootfs\|materialize\|error\|libkrun' | head -3 >&2
  exit 1
}
preflight

MODELS=("$@")
if [ "${#MODELS[@]}" -eq 0 ]; then
  MODELS=(
    opencode-go/glm-5.2
    opencode-go/kimi-k2.7-code
    opencode-go/deepseek-v4-pro
    opencode-go/qwen3.7-max
    opencode-go/minimax-m3
  )
fi

slug() { printf '%s' "$1" | tr '/' '_' | tr -c 'A-Za-z0-9_.-' '_'; }

echo "H5 cross-model σ̂ — ${#MODELS[@]} models × 3 tasks × 3 arms × $TRIALS trials (PARALLEL)"
echo "  binary: $PILLBOX"
echo "  results: $RESULTS"
echo

# Orphan-VMM reaper alongside the run: 3× parallelism = 3× the churn-wedge rate,
# so a safety-net sweep keeps a long unattended batch from accumulating orphans
# (see reap-orphan-vmms.sh). Killed on driver exit. DEDICATED-host only.
PILLBOX="$PILLBOX" "$here/../reap-orphan-vmms.sh" > "$RESULTS/reaper.log" 2>&1 &
REAPER_PID=$!
trap 'kill "$REAPER_PID" 2>/dev/null' EXIT
echo "  reaper: pid $REAPER_PID → $RESULTS/reaper.log"
echo

# Models are independent (own OUT file, own model, per-session UUID paths) → run
# them concurrently. Each run.sh is serial WITHIN a model (~1-2 VMs in flight), so
# N models ≈ N×2 concurrent VMs — well within host RAM at this panel size.
pids=()
for m in "${MODELS[@]}"; do
  out="$RESULTS/h5-$(slug "$m")-n${TRIALS}.jsonl"
  log="$RESULTS/h5-$(slug "$m")-n${TRIALS}.log"
  if [ -s "$out" ]; then echo "▷ skip $m (have $(wc -l <"$out") records in $out)"; continue; fi
  echo "▶ launching $m → $out (log: $log)"
  ( PILLBOX="$PILLBOX" MODEL="$m" TRIALS="$TRIALS" OUT="$out" "$here/run.sh" \
      || echo "  ! run.sh exited non-zero for $m (partial records kept)" ) > "$log" 2>&1 &
  pids+=($!)
done
echo "  ${#pids[@]} models running in parallel; waiting for all…"
for p in "${pids[@]}"; do wait "$p"; done

echo
echo "==================== per-model summary ===================="
for m in "${MODELS[@]}"; do
  out="$RESULTS/h5-$(slug "$m")-n${TRIALS}.jsonl"
  [ -s "$out" ] || { echo "$m: (no records)"; continue; }
  echo "--- $m ---"
  # monolithic = the raw-model σ̂/mean (the headroom gate); segmented = the lever.
  python3 "$here/../paired-stats.py" --baseline monolithic --treatment segmented "$out" 2>/dev/null \
    || echo "  (paired-stats failed — inspect $out)"
done
