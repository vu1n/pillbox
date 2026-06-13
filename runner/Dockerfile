# syntax=docker/dockerfile:1
# Runner image for pillbox sandboxes.
#
# Five agent harnesses preinstalled at known paths:
#   - claude  (Anthropic's native installer → /usr/local/bin/claude)
#   - codex   (OpenAI's official installer → /usr/local/bin/codex)
#   - amp     (npm: @ampcode/cli)
#   - opencode (npm: opencode-ai)
#   - pi      (npm: @earendil-works/pi-coding-agent)
#
# System tooling chosen to cover the agents' subagent + shell needs:
# bubblewrap (codex/claude subagent sandboxing), git/gh (vcs ops),
# ripgrep (search), tmux (multi-pane TUI), jq (json scripting),
# gawk (codex installer's SHA256 manifest check uses interval regexes
# that Debian's default mawk doesn't support),
# python3 + pip/venv (script runner; grader test-deps via `session score
# --grader-egress` — bookworm pip is PEP-668 externally-managed, so graders
# use a venv), openssl (vault CA verification),
# curl/ca-certificates (pulls + TLS).
#
# Contract assumed by pillbox CLI:
#   - Agent binaries are on $PATH (verified by `pillbox doctor`)
#   - /workspace exists and is writable (bind-mount target)
#   - /etc is writable for the --mcp-config bind mount
#   - HOME is set by the caller (pillbox sets HOME=/home/pillbox at runtime
#     and bind-mounts the agent's persistent auth state there)
#   - A shell is available
#
# To layer your own tools, FROM this image and apt-get / npm-install on top:
#   FROM ghcr.io/vu1n/pillbox-runner:vX.Y.Z
#   RUN apt-get update && apt-get install -y my-tool && rm -rf /var/lib/apt/lists/*
#
# NOTE: the build context is the REPO ROOT (not runner/), because the
# pillbox binary is compiled in-image below:
#   docker buildx build -f runner/Dockerfile .

# ---- build stage: compile the in-sandbox pillbox binary ----------------
# The runner ships `pillbox` itself: the interactive attach pty-host
# (`pillbox pty-host`) and the in-sandbox emitter / `session done` wrapper
# all run *inside* the sandbox, matching what the e2b/ssh backends already
# assume. Same glibc (bookworm) as the runtime stage below.
FROM rust:1-bookworm AS pillbox-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache mounts keep the cargo registry + compiled deps on the build daemon, so
# a rebuild on the same host only recompiles changed crates (minutes → seconds
# on iteration — the BYO/on-target build loop). The `runner` profile skips
# release LTO for a much faster compile (see Cargo.toml). `cp` runs inside the
# same RUN so it grabs the binary before the target cache mount detaches.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --profile runner --locked && cp target/runner/pillbox /pillbox

# ---- runtime stage -----------------------------------------------------
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV NPM_CONFIG_UPDATE_NOTIFIER=false
ENV NODE_ENV=production
# Agents/graders run with an explicit PATH from `agents::guest_path()`
# (/usr/local/bin:/usr/bin:/bin:$HOME/.local/bin). Install agent binaries onto
# one of those dirs (this image uses /usr/local/bin) or guest_path() must change.
ENV PATH="/usr/local/bin:${PATH}"

RUN apt-get update \
	&& apt-get install -y --no-install-recommends \
		bash \
		bubblewrap \
		ca-certificates \
		curl \
		gawk \
		gh \
		git \
		iproute2 \
		jq \
		openssl \
		python3 \
		python3-pip \
		python3-venv \
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
# renovate: datasource=github-releases depName=openai/codex extractVersion=^rust-v(?<version>.*)$
ARG CODEX_VERSION=0.139.0
# renovate: datasource=npm depName=opencode-ai
ARG OPENCODE_VERSION=1.17.3
# renovate: datasource=npm depName=@earendil-works/pi-coding-agent
ARG PI_VERSION=0.79.1

RUN npm install -g \
		@ampcode/cli@latest \
		opencode-ai@${OPENCODE_VERSION} \
		@earendil-works/pi-coding-agent@${PI_VERSION} \
	&& npm cache clean --force
	# ^ the global install leaves a populated npm cache (~/.npm) in the
	#   layer that nothing reads at runtime — drop it.

# Codex: official standalone installer (native binary; the npm wrapper is
# no longer how upstream ships protocol-bearing builds like `app-server`).
# Same scratch-HOME pattern as Claude below: the installer symlinks
# ~/.local/bin/codex into $CODEX_HOME/packages/standalone/releases/<ver>,
# so resolve the link with `cp -L` into /usr/local/bin and drop the
# scratch tree.
RUN curl -fsSL https://chatgpt.com/codex/install.sh -o /tmp/codex-install.sh \
	&& HOME=/opt/codex-install CODEX_RELEASE=${CODEX_VERSION} CODEX_NON_INTERACTIVE=1 \
		sh /tmp/codex-install.sh \
	&& cp -L /opt/codex-install/.local/bin/codex /usr/local/bin/codex \
	&& chmod +x /usr/local/bin/codex \
	&& rm /tmp/codex-install.sh \
	&& rm -rf /opt/codex-install

# Claude Code: official native installer. Run with HOME pointed at a
# stable /opt path so the installer's writes don't get shadowed if a
# downstream consumer mounts a tmpfs over /root or /home. Copy the
# resolved binary into /usr/local/bin so it survives any HOME shuffle.
RUN curl -fsSL https://claude.ai/install.sh -o /tmp/claude-install.sh \
	&& HOME=/opt/claude-install bash /tmp/claude-install.sh \
	&& cp -L /opt/claude-install/.local/bin/claude /usr/local/bin/claude \
	&& chmod +x /usr/local/bin/claude \
	&& rm /tmp/claude-install.sh \
	&& rm -rf /opt/claude-install
	# ^ the installer's scratch HOME is a full second copy of Claude Code
	#   (its bundled runtime + node_modules, ~250MB). We've copied the
	#   resolved binary into /usr/local/bin, so drop the scratch dir —
	#   otherwise it rides along dead in the layer.

# The in-sandbox pillbox: pty-host, event emitter, session-done wrapper.
COPY --from=pillbox-build /pillbox /usr/local/bin/pillbox

# Boot hook that installs the vault CA into the system trust store when
# pillbox bind-mounts one — required for non-Node agents (Codex's
# reqwest, future Rust/Go agents) to honor the MITM cert. Node agents
# go through `NODE_EXTRA_CA_CERTS` and don't need this. See
# `runner/entrypoint.sh` for the full rationale; the script is a
# no-op on non-vaulted runs.
COPY runner/entrypoint.sh /usr/local/bin/pillbox-entrypoint.sh
RUN chmod +x /usr/local/bin/pillbox-entrypoint.sh
ENTRYPOINT ["/usr/local/bin/pillbox-entrypoint.sh"]

WORKDIR /workspace

# `sleep infinity` for `docker exec`-style consumers; pillbox CLI
# overrides with the agent command via `docker run … claude` etc.
# The entrypoint above runs first, then execs whatever CMD lands here.
CMD ["sleep", "infinity"]
