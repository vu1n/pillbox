#!/usr/bin/env bash
# CF §0 gateway smokes: spin `wrangler dev` (workerd — no container needed for the
# §0-only path) and run the auth / driver-arbitration / annotation .mjs smokes
# against it. Covers the managed-placement trust boundary that unit CI can't reach.
#
#   Usage: scripts/smoke/cf.sh
# Env: SMOKE_CF_PORT (default 8799).
# Prereqs: cloudflare-spike deps installed (npm i), Node >= 23 (for .ts imports).
set -uo pipefail
cd "$(dirname "$0")/../../cloudflare-spike"

PORT="${SMOKE_CF_PORT:-8799}"
# A dev secret so the actor tokens verify (gitignored; never a real secret).
[ -f .dev.vars ] || echo "ACTOR_TOKEN_SECRET=dev-secret-for-local-only" >.dev.vars

echo "▶ CF §0 smokes (wrangler dev :$PORT)"
npx tsc --noEmit || { echo "  ✗ tsc --noEmit failed"; exit 1; }
echo "  ✓ tsc clean"

pkill -9 -f "wrangler dev" 2>/dev/null
sleep 1
npx wrangler dev --port "$PORT" >/tmp/smoke-wrangler.log 2>&1 &
cleanup() { pkill -9 -f "wrangler dev" 2>/dev/null; }
trap cleanup EXIT

# Wait for workerd to answer (404 on / is expected — the worker is up).
ready=""
for _ in $(seq 1 30); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/" 2>/dev/null && { ready=1; break; }
  sleep 2
done
[ -n "$ready" ] || { tail -5 /tmp/smoke-wrangler.log; echo "  ✗ wrangler dev never came up"; exit 1; }

node test-auth.mjs || { echo "  ✗ test-auth (token crypto)"; exit 1; }
node smoke-actor.mjs "http://127.0.0.1:$PORT" || { echo "  ✗ smoke-actor (attestation / write-auth)"; exit 1; }
node smoke-driver.mjs "http://127.0.0.1:$PORT" || { echo "  ✗ smoke-driver (arbitration / annotation)"; exit 1; }
echo "  ✓✓ CF PASS"
