#!/usr/bin/env bash
# The live smoke gate — run before merging anything that touches the substrate.
# Unit CI (cargo test on macos+ubuntu) can't reach the real paths: libkrun VM
# boot, agent drive, the §0 producers, session pull, the CF §0 gateway. This does.
# It builds + codesigns the libkrun binary first, then exercises those paths
# end-to-end.
#
#   Usage: scripts/smoke/run.sh [all|libkrun|cf]   (default: all)
# Env:
#   SMOKE_CODEX=1            also run the codex-serve libkrun smoke (opt-in: its
#                            bring-up is more sensitive + needs the l8 image, so
#                            it's off by default to keep the gate cry-wolf-free).
#   OPENCODE_IMAGE           runner image for opencode  (default pillbox-runner:l7)
#   CODEX_IMAGE              runner image for codex-serve (default pillbox-runner:l8)
#   SMOKE_MODEL              model for opencode (default zai-coding-plan/glm-4.5-air)
set -uo pipefail
cd "$(dirname "$0")/../.."

which="${1:-all}"
rc=0

if [ "$which" = all ] || [ "$which" = libkrun ]; then
  echo "== libkrun build + codesign =="
  scripts/lk-build.sh || exit 1
  scripts/smoke/libkrun.sh opencode "${OPENCODE_IMAGE:-pillbox-runner:l7}" \
    "${SMOKE_MODEL:-zai-coding-plan/glm-4.5-air}" || rc=1
  # The PTY drive+read keystone: pty_send → guest PTY, creds_share transcript →
  # §0 log, wait-idle. A real claude PTY turn (driving a TUI agent) — the path
  # server-mode can't reach.
  scripts/smoke/libkrun-pty.sh claude "${OPENCODE_IMAGE:-pillbox-runner:l7}" || rc=1
  if [ "${SMOKE_CODEX:-0}" = 1 ]; then
    scripts/smoke/libkrun.sh codex-serve "${CODEX_IMAGE:-pillbox-runner:l8}" || rc=1
  else
    echo "  · codex-serve smoke skipped (set SMOKE_CODEX=1 to include it)"
  fi
  # The dispatch fan-out (GHOST-004): forks 2 opencode workers, then drives /
  # scores / pulls them — boots ~2 VMs, so it runs after the single-agent smoke.
  scripts/smoke/dispatch.sh "${OPENCODE_IMAGE:-pillbox-runner:l7}" \
    "${SMOKE_MODEL:-zai-coding-plan/glm-4.5-air}" || rc=1
fi

if [ "$which" = all ] || [ "$which" = cf ]; then
  scripts/smoke/cf.sh || rc=1
fi

echo
[ "$rc" = 0 ] && echo "✓✓✓ SMOKE PASS" || echo "✗✗✗ SMOKE FAILED"
exit "$rc"
