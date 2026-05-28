#!/bin/sh
# Pillbox runner entrypoint.
#
# When pillbox stands up a vault session, it bind-mounts the per-run
# CA certificate at /usr/local/share/ca-certificates/pillbox-vault.crt.
# Node-based agents (Claude Code, opencode, pi) honor that via the
# `NODE_EXTRA_CA_CERTS` env pillbox also sets, but Rust/Go agents
# (Codex's reqwest, future agents) only consult the system trust
# store. Run `update-ca-certificates` once at boot when the CA is
# present so those clients trust the MITM cert too.
#
# - Idempotent: re-running on subsequent reattaches is a no-op
#   beyond a trust-bundle regeneration (~50ms).
# - Silent on success so non-vaulted runs see clean agent stdout
#   from the first line. Failure warns to stderr but does not abort
#   — the agent can still run; only TLS-strict Rust/Go clients will
#   fail downstream, which is what the smoke test was already
#   surfacing.
# - Skips entirely when no CA was mounted: non-vaulted `pillbox run`
#   pays nothing.
#
# After the cert step, `exec "$@"` hands control to the original CMD
# (pillbox passes the agent command, e.g. `claude` / `codex` / `sleep
# infinity`).
set -e

PILLBOX_CA=/usr/local/share/ca-certificates/pillbox-vault.crt
if [ -r "$PILLBOX_CA" ]; then
    update-ca-certificates >/dev/null 2>&1 || \
        echo "pillbox: warning: update-ca-certificates failed; non-Node agents (Codex etc.) may reject the vault TLS cert" >&2
fi

exec "$@"
