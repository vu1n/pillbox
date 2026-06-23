#!/usr/bin/env bash
# Live e2e smoke for `pillbox dispatch --agent claude` — the one-shot CLI-harness
# drive path. claude is `claude -p PROMPT` (one-shot): its prompt must ride the
# LAUNCH argv, not a later `session send` (the server/opencode model). dispatch now
# forks CLI workers WITH the prompt baked in (`run --detach -- "<prompt>"`) and just
# waits+grades — the fix for the detach-then-send model that booted a VM but never
# ran a turn (→ silent 0/0). The unit tests cover the policy on a mock; this covers
# the REAL claude/Opus drive on a booted VM (the GHOST-004 analog for CLI agents).
# Also exercises 2 CONCURRENT claude forks through the vault (the degraded-lease /
# coalesce path), so it doubles as a fork-k-claude vault check.
#
#   Usage: scripts/smoke/dispatch-claude.sh [runner-image]
# Prereqs: codesigned libkrun binary (scripts/lk-build.sh), claude authed
# (`pillbox auth login --agent claude`), the runner image present, Opus reachable.
set -euo pipefail
cd "$(dirname "$0")/../.." || exit 1

IMAGE="${1:-pillbox-runner:dev}"
BACKEND="${PILLBOX_BACKEND:-libkrun}"
PB="$(pwd)/target/debug/pillbox"
export PILLBOX_BACKEND="$BACKEND" PILLBOX_RUNNER_IMAGE="$IMAGE"

fail() { echo "  ✗ dispatch-claude: $1"; exit 1; }

# libkrun-only (same grade-path constraint as dispatch.sh): the grader resolves each
# worker's live workspace via `session info --json` → `.session.workspace`, libkrun
# only. Skip (don't fail the suite) on docker.
if [ "$BACKEND" != libkrun ]; then
  echo "  · dispatch-claude smoke skipped — backend=$BACKEND not wired (libkrun-only grade path)"
  exit 0
fi
[ "$(nm "$PB" 2>/dev/null | grep -c LibkrunBackend)" -ge 1 ] \
  || fail "binary lacks the libkrun feature — run scripts/lk-build.sh"

WS="$(mktemp -d)"
cleanup() {
  if cd "$WS" 2>/dev/null; then
    for s in $("$PB" session list --json 2>/dev/null | jq -r '.sessions[].id' 2>/dev/null); do
      "$PB" session rm "$s" >/dev/null 2>&1
    done
    "$PB" rm dispatch-claude-smoke >/dev/null 2>&1
  fi
  cd / && rm -rf "$WS"
}
trap cleanup EXIT

cd "$WS" || fail "cd into workspace $WS failed"
"$PB" new --name dispatch-claude-smoke >/dev/null 2>&1 || fail "pillbox new failed"
echo seed >seed.txt
"$PB" push --bookmark base >/dev/null 2>&1 || fail "push --bookmark failed"

echo "▶ dispatch-claude smoke (image=$IMAGE, k=2 claude/Opus, prompt-at-launch)"
# k=2, no --model → claude's subscription default (Opus). A trivial, deterministic
# task; `--cmd` grades the pulled workspace (the fix is the DRIVE, not cleverness).
OUT="$(PILLBOX_DISPATCH_TURN_TIMEOUT=600 "$PB" dispatch --from-bookmark base \
  -k 2 --agent claude \
  --cmd 'test -f result.txt && grep -qi done result.txt' --json \
  -- 'Create a file named result.txt containing exactly the word DONE' 2>/tmp/dispatch-claude.err)"
RC=$?

echo "$OUT" | jq . >/dev/null 2>&1 || { tail -6 /tmp/dispatch-claude.err; fail "verdict is not valid JSON: $OUT"; }
[ "$(echo "$OUT" | jq -r '.version')" = 1 ] || fail "verdict version != 1"
N="$(echo "$OUT" | jq -r '.dispatch.workers | length')"
[ "$N" -eq 2 ] || fail "expected 2 workers, got $N"

# The crux: a CLI agent that was actually DRIVEN produces a gradeable result. Before
# the fix every worker errored/0 (booted, never ran a turn). Assert ≥1 scored.
SCORED="$(echo "$OUT" | jq -r '[.dispatch.workers[] | select(.status=="scored")] | length')"
echo "  · $SCORED/2 workers scored"
[ "$SCORED" -ge 1 ] || { echo "$OUT" | jq .; tail -6 /tmp/dispatch-claude.err; fail "no worker drove a turn (the pre-fix 0/0 symptom)"; }

WINNER="$(echo "$OUT" | jq -r '.dispatch.winner // empty')"
[ -n "$WINNER" ] || { echo "$OUT" | jq .; fail "no winner (rc=$RC)"; }
echo "  ✓ winner selected: $WINNER"

PULLED="$(echo "$OUT" | jq -r '.dispatch.pulled_to // empty')"
{ [ -n "$PULLED" ] && [ -d "$PULLED" ]; } || fail "winner not pulled (pulled_to=$PULLED)"
grep -qi 'done' "$PULLED/result.txt" 2>/dev/null \
  || fail "pulled winner lacks result.txt with DONE — got: $(cat "$PULLED/result.txt" 2>/dev/null | tr '\n' ' ')"
echo "  ✓ winner pulled → result.txt recovered (claude/Opus actually ran a turn)"

[ "$RC" -eq 0 ] || fail "winner found but exit code was $RC (want 0)"
echo "  ✓✓ dispatch-claude PASS"
