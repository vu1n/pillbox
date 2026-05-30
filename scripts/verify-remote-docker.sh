#!/usr/bin/env bash
# Tier-3 verification for the docker:// remote backend: the agent + vault
# round-trip end-to-end against a REAL Docker host. This is NOT a CI test — it
# needs `pillbox auth login` credentials and a reachable host — so it's a
# repeatable runbook you invoke on demand (before releases, or when the
# docker:// path changes). The CI-able mechanism tests live in the
# `docker-mechanism` job (.github/workflows/ci.yml).
#
# What it proves on the wire:
#   - placement: pillbox reaches the daemon over DOCKER_HOST=ssh://…
#   - image distribution: a from-branch runner image is built natively on the
#     target (so the in-container pillbox matches host-side — the version-skew
#     trap), rather than save|load'ing a wrong-arch local image
#   - the run assembly: create → tar-cp stage → start → the agent runs
#   - vault: the agent authenticates through the sandbox-side proxy (creds
#     forwarded via the blob), i.e. "vault just works" with no host-side proxy
#   - I6 sovereignty: the workspace `.env` is excluded from the staged transfer
#
# Usage:
#   pillbox auth login --agent claude        # once — Tier 3 needs real creds
#   REMOTE=docker://user@host scripts/verify-remote-docker.sh
#
# Env:
#   REMOTE   (required)  docker://[user@]host[:port]
#   AGENT    (default claude)
#   IMAGE    (default pillbox-runner:verify)  tag for the from-branch image
set -euo pipefail

REMOTE="${REMOTE:?set REMOTE=docker://[user@]host[:port]}"
AGENT="${AGENT:-claude}"
IMAGE="${IMAGE:-pillbox-runner:verify}"

case "$REMOTE" in
  docker://*) DEST="${REMOTE#docker://}" ;;
  *) echo "REMOTE must be a docker:// URL, got: $REMOTE" >&2; exit 2 ;;
esac
DOCKER_HOST_SSH="ssh://${DEST}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

echo "==> [1/4] building host pillbox (release)"
cargo build --release --bin pillbox >/dev/null
PILLBOX="$repo_root/target/release/pillbox"

echo "==> [2/4] building the from-branch runner image natively on $DOCKER_HOST_SSH"
# DOCKER_HOST ships only the (small, .dockerignore'd) source context; the host
# compiles arch-native — the correct BYO pattern vs save|load'ing a wrong-arch
# local image. CAVEAT: buildkit-over-ssh intermittently fails to establish its
# session ("no active session" / "file already closed" / "context deadline
# exceeded") — it's transient, so retry. (For production BYO, prefer publishing
# a prebuilt multi-arch image and `docker pull`ing it on the host — build-on-
# target over ssh is convenient but flaky; see docs/remotes.md.)
built=0
for attempt in 1 2 3; do
  if DOCKER_HOST="$DOCKER_HOST_SSH" docker build -f runner/Dockerfile -t "$IMAGE" . >/dev/null 2>&1; then
    built=1; break
  fi
  echo "    build attempt $attempt failed (likely buildkit-over-ssh flake); retrying…" >&2
  sleep 3
done
[ "$built" -eq 1 ] || { echo "image build failed after 3 attempts" >&2; exit 1; }
echo "    built $IMAGE on the remote daemon"

echo "==> [3/4] running a headless agent against $REMOTE"
ws="$(mktemp -d)"
trap 'rm -rf "$ws"' EXIT
printf 'hello from the verification harness\n' > "$ws/README.md"
printf 'SECRET=must-not-ship\n' > "$ws/.env"           # the I6 canary
git -C "$ws" init -q >/dev/null 2>&1 || true

set +e
out="$(cd "$ws" && PILLBOX_RUNNER_IMAGE="$IMAGE" "$PILLBOX" \
  run --remote "$REMOTE" --agent "$AGENT" -- -p "reply with exactly one word: hello" </dev/null 2>&1)"
rc=$?
set -e
echo "----- run output -----"
echo "$out"
echo "----------------------"

echo "==> [4/4] assertions"
fail=0
# I6: the workspace .env must have been dropped from the staged transfer.
if echo "$out" | grep -q "secret path(s) excluded"; then
  echo "  PASS  .env excluded from the staged workspace (I6 on the wire)"
else
  echo "  FAIL  no secret-exclusion note — .env may have shipped"; fail=1
fi
# The agent ran to completion (a non-zero exit would be auth/vault failure).
if [ "$rc" -eq 0 ]; then
  echo "  PASS  agent run completed cleanly (vault round-trip ok)"
else
  echo "  FAIL  agent exited non-zero ($rc) — likely auth/vault"; fail=1
fi

echo
echo "NOTE: streaming the agent's *output* (the model's reply) is the deferred"
echo "      result-capture slice; a fast headless agent exits before the PTY"
echo "      attach connects. For a deeper check, re-run interactively, or"
echo "      inspect the container transcript before reap (see docs/remotes.md)."

[ "$fail" -eq 0 ] && echo "✓ docker:// verification PASSED" || { echo "✗ docker:// verification FAILED"; exit 1; }
