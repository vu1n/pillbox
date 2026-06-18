# The Sandbox SDK's prebuilt container server (default variant: node/bun + the
# exec/files/process control plane on :3000). The DO drives it via getSandbox().
# Bump in lockstep with the @cloudflare/sandbox version in package.json.
FROM docker.io/cloudflare/sandbox:0.11.0

# opencode CLI — createOpencodeServer runs `opencode serve` and expects the binary
# on PATH; the base sandbox image ships node/npm/bun but NOT opencode (a missing
# binary is the OpencodeStartupError "Stderr: (empty)"). Pinned to the version
# pillbox's runner verified against the zai-coding-plan provider + /event +
# /prompt_async (runner/Dockerfile OPENCODE_VERSION).
RUN npm install -g opencode-ai@1.17.3

# The opencode session's working directory (createOpencodeServer's `directory`,
# == OPENCODE_DIR in session_gateway.ts). Created so the consume path has a writable cwd.
RUN mkdir -p /workspace
