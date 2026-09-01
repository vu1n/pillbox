# The Sandbox SDK's prebuilt container server (default variant: node/bun + the
# exec/files/process control plane on :3000). The DO drives it via getSandbox().
# Bump in lockstep with the @cloudflare/sandbox version in package.json.
FROM docker.io/cloudflare/sandbox:0.11.0

# opencode CLI — createOpencodeServer runs `opencode serve` and expects the binary
# on PATH; the base sandbox image ships node/npm/bun but NOT opencode (a missing
# binary is the OpencodeStartupError "Stderr: (empty)"). Pinned to the version
# pillbox's runner verified against the zai-coding-plan provider + /event +
# /prompt_async (runner/Dockerfile OPENCODE_VERSION).
RUN npm install -g opencode-ai@1.18.9

# The pillbox binary (linux/amd64) — the managed tier's workspace tool. The
# The managed Worker execs `pillbox workspace restore|backup` to rehydrate the
# rustic-on-R2 workspace into /workspace before the agent runs and snapshot
# results back after. Cross-built against glibc 2.31 (≤ this image's 2.35, so it
# runs) from the repo root and dropped in this build context; gitignored, so it
# must be (re)built + copied before `wrangler deploy` (see README).
COPY pillbox-linux-amd64 /usr/local/bin/pillbox
RUN chmod +x /usr/local/bin/pillbox

# The opencode session's working directory (createOpencodeServer's `directory`,
# == OPENCODE_DIR in opencode_turn.ts). Created so the runtime has a writable cwd.
RUN mkdir -p /workspace
