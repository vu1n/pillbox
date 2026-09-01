#!/usr/bin/env bash
# Run the docker:// workspace-staging + container-lifecycle mechanism tests
# against a real Docker daemon. These are `#[ignore]`d (they need a daemon) and
# deliberately kept OUT of GitHub CI (a docker job burns free minutes) — run
# them here locally, or on a self-hosted box, before touching the docker://
# path.
#
# They only need tar/sleep/test, so a tiny image stands in for the full runner
# (override with PILLBOX_TEST_RUNNER_IMAGE). This guards the create→stage→start
# ordering and the secret-denylist on the actual wire — what the unit suite
# (no daemon) can't.
#
# Usage:
#   scripts/test-docker-mechanism.sh
#   DOCKER_HOST=ssh://user@host scripts/test-docker-mechanism.sh   # against a remote daemon
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if ! docker version >/dev/null 2>&1; then
  echo "no reachable Docker daemon (set DOCKER_HOST, or start Docker)" >&2
  exit 4
fi

export PILLBOX_TEST_RUNNER_IMAGE="${PILLBOX_TEST_RUNNER_IMAGE:-busybox:latest}"
echo "==> docker:// mechanism tests against image: $PILLBOX_TEST_RUNNER_IMAGE"
cargo test --bin pillbox -- --ignored sandbox::workspace_stage sandbox::container
