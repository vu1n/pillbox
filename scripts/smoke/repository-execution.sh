#!/usr/bin/env bash
# Offline preparation is the default. Only explicit live/verify-retry run Pillbox.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
exec python3 -E -s "$HERE/repository-execution/smoke.py" "$@"
