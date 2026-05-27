# Runner image for pillbox sandboxes.
#
# Five agent harnesses preinstalled at known paths:
#   - claude  (Anthropic's native installer → /usr/local/bin/claude)
#   - codex   (npm: @openai/codex)
#   - amp     (npm: @ampcode/cli)
#   - opencode (npm: opencode-ai)
#   - pi      (npm: @earendil-works/pi-coding-agent)
#
# System tooling chosen to cover the agents' subagent + shell needs:
# bubblewrap (codex/claude subagent sandboxing), git/gh (vcs ops),
# ripgrep (search), tmux (multi-pane TUI), jq (json scripting),
# python3 (script runner), openssl (vault CA verification),
# curl/ca-certificates (pulls + TLS).
#
# Contract assumed by pillbox CLI:
#   - Agent binaries are on $PATH (verified by `pillbox doctor`)
#   - /workspace exists and is writable (bind-mount target)
#   - /etc is writable for the --mcp-config bind mount
#   - HOME is set by the caller (pillbox sets HOME=/home/lum at runtime
#     and bind-mounts the agent's persistent auth state there)
#   - A shell is available
#
# To layer your own tools, FROM this image and apt-get / npm-install on top:
#   FROM ghcr.io/vu1n/pillbox-runner:vX.Y.Z
#   RUN apt-get update && apt-get install -y my-tool && rm -rf /var/lib/apt/lists/*

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV NPM_CONFIG_UPDATE_NOTIFIER=false
ENV NODE_ENV=production
ENV PATH="/usr/local/bin:${PATH}"

RUN apt-get update \
	&& apt-get install -y --no-install-recommends \
		bash \
		bubblewrap \
		ca-certificates \
		curl \
		gh \
		git \
		jq \
		openssl \
		python3 \
		ripgrep \
		tmux \
		xz-utils \
	&& rm -rf /var/lib/apt/lists/*

# Node 22 LTS from NodeSource — Debian's `nodejs` package lags behind
# what the agent CLIs require.
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
	&& apt-get update \
	&& apt-get install -y --no-install-recommends nodejs \
	&& rm -rf /var/lib/apt/lists/*

# Agent-harness version pins. Renovate (see renovate.json at repo
# root) tracks each one via the `# renovate:` hint comments below
# and opens PRs on upstream bumps. amp stays on @latest because its
# publish scheme is timestamp+sha (e.g. 0.0.1779836786-gb003ec),
# which Renovate's semver matcher can't compare meaningfully.
# renovate: datasource=npm depName=@openai/codex
ARG CODEX_VERSION=0.134.0
# renovate: datasource=npm depName=opencode-ai
ARG OPENCODE_VERSION=1.15.10
# renovate: datasource=npm depName=@earendil-works/pi-coding-agent
ARG PI_VERSION=0.75.5

RUN npm install -g \
		@openai/codex@${CODEX_VERSION} \
		@ampcode/cli@latest \
		opencode-ai@${OPENCODE_VERSION} \
		@earendil-works/pi-coding-agent@${PI_VERSION}

# Claude Code: official native installer. Run with HOME pointed at a
# stable /opt path so the installer's writes don't get shadowed if a
# downstream consumer mounts a tmpfs over /root or /home. Copy the
# resolved binary into /usr/local/bin so it survives any HOME shuffle.
RUN curl -fsSL https://claude.ai/install.sh -o /tmp/claude-install.sh \
	&& HOME=/opt/claude-install bash /tmp/claude-install.sh \
	&& cp -L /opt/claude-install/.local/bin/claude /usr/local/bin/claude \
	&& chmod +x /usr/local/bin/claude \
	&& rm /tmp/claude-install.sh

WORKDIR /workspace

# `sleep infinity` for `docker exec`-style consumers; pillbox CLI
# overrides with the agent command via `docker run … claude` etc.
CMD ["sleep", "infinity"]
