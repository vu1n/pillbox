#!/usr/bin/env bash
# Cloudflare managed-boundary smoke: spin `wrangler dev` without a container,
# verify capability crypto, and prove the live public execution route fails
# closed before it can touch D1.
#
#   Usage: scripts/smoke/cf.sh
# Env: SMOKE_CF_PORT (default 8799).
# Prereqs: cloudflare-spike deps installed (npm i), Node >= 22.6 (type stripping).
set -uo pipefail
cd "$(dirname "$0")/../../cloudflare-spike"

PORT="${SMOKE_CF_PORT:-8799}"
echo "▶ CF managed smokes (wrangler dev :$PORT)"
# §0 contract parity FIRST — pure file-parse, no deps, fails fast before spinning wrangler.
# Guards "one §0, two backends": contract.ts must stay faithful to contract.rs.
python3 check-contract-parity.py || { echo "  ✗ §0 contract parity drift (see above)"; exit 1; }
echo "  ✓ §0 contract parity holds"
npx tsc --noEmit || { echo "  ✗ tsc --noEmit failed"; exit 1; }
echo "  ✓ tsc clean"

npx wrangler dev \
  --config wrangler.runtime-test.toml \
  --var MANAGED_CAPABILITY_SECRET:dev-secret-for-local-only \
  --port "$PORT" >/tmp/smoke-wrangler.log 2>&1 &
wrangler_pid=$!
cleanup() { kill "$wrangler_pid" 2>/dev/null; }
trap cleanup EXIT

# Wait for workerd to answer (404 on / is expected — the worker is up).
ready=""
for _ in $(seq 1 30); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/" 2>/dev/null && { ready=1; break; }
  sleep 2
done
[ -n "$ready" ] || { tail -5 /tmp/smoke-wrangler.log; echo "  ✗ wrangler dev never came up"; exit 1; }

node --test test-auth.mjs || { echo "  ✗ test-auth (capability crypto)"; exit 1; }

status="$(curl -s -o /tmp/smoke-managed-response.json -w '%{http_code}' \
  -H 'content-type: application/json' \
  --data '{"invocation_id":"smoke-invocation","session_ref":{"session_id":"smoke-session"}}' \
  "http://127.0.0.1:$PORT/v2/executions")"
[ "$status" = "401" ] || {
  cat /tmp/smoke-managed-response.json
  echo "  ✗ public execution did not fail closed (HTTP $status)"
  exit 1
}
echo "  ✓ unauthenticated public execution fails closed"
echo "  ✓✓ CF PASS"
