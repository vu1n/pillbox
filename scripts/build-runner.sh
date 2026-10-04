#!/usr/bin/env bash
# Build the pillbox runner image — the source rootfs for the local backends
# (libkrun materializes its microVM rootfs from it; the deprecated docker
# backend runs it directly). This is the one-stop way to UPDATE THE BUNDLED
# AGENTS: `--update` resolves the latest version of each agent harness, rewrites
# the version pins in runner/Dockerfile, rebuilds, and prints the versions
# actually baked into the image.
#
# Why a script and not `pillbox <subcommand>`: the build needs the repo (the
# Dockerfile + the cargo source for the in-sandbox binary), so it's a dev tool,
# not a runtime command — same shape as scripts/lk-build.sh.
#
# Agent updates are deliberate, tracked changes: --update edits the pins in
# runner/Dockerfile, so review `git diff runner/Dockerfile` and commit — same
# model as a Renovate bump. Layer caching keeps the rebuild partial: apt / Node /
# the cargo-built pillbox layers stay cached, only the bumped agent layers
# recompile. (Pinning is what makes that correct — an unpinned `@latest` rides a
# RUN whose command string never changes, so the cache serves a stale agent.)
set -euo pipefail
cd "$(dirname "$0")/.."

DOCKERFILE=runner/Dockerfile
TAG=pillbox-runner:dev   # moving dev tag — every local script + a dev pillbox.toml default to it
ROOTFS_CACHE_VERSION=v5  # mirrors ROOTFS_CACHE_VERSION in src/sandbox/libkrun/mod.rs
DO_UPDATE=0 DRY_RUN=0 NO_CACHE=0 PRUNE=0 PRINT_ROOTFS_NAMESPACE=

usage() {
	cat <<'EOF'
Usage: scripts/build-runner.sh [options]

  (no options)       rebuild the current pins (layer-cached) and verify
  -u, --update       resolve each agent's latest version, rewrite the pins in
                     runner/Dockerfile, rebuild, verify
      --dry-run      with --update: print what would change, don't write or build
  -t, --tag TAG      image tag to build (default: pillbox-runner:dev)
      --no-cache     force a clean rebuild (pass --no-cache to docker)
      --prune-rootfs reserved: pruning mounted rootfs backing requires a
                     validated maintenance operation; this flag fails closed
      --print-rootfs-cache-namespace IMAGE
                     print IMAGE's current cache namespace and exit (no build)
  -h, --help         this help

After --update, review `git diff runner/Dockerfile` and commit the bumped pins.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
		-u|--update)     DO_UPDATE=1 ;;
		--dry-run)       DRY_RUN=1 ;;
		--no-cache)      NO_CACHE=1 ;;
		--prune-rootfs)  PRUNE=1 ;;
		--print-rootfs-cache-namespace)
			PRINT_ROOTFS_NAMESPACE="${2:?--print-rootfs-cache-namespace needs an image}"
			shift
			;;
		-t|--tag)        TAG="${2:?--tag needs a value}"; shift ;;
		-h|--help)       usage; exit 0 ;;
		*) echo "✗ unknown arg: $1" >&2; usage >&2; exit 2 ;;
	esac
	shift
done

if [ "$PRUNE" = 1 ]; then
	echo "✗ --prune-rootfs is unavailable: mounted rootfs backing requires validated maintenance" >&2
	exit 2
fi

sha256_text() {
	if command -v sha256sum >/dev/null 2>&1; then
		printf '%s' "$1" | sha256sum | awk '{print $1}'
	elif command -v shasum >/dev/null 2>&1; then
		printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
	else
		echo "✗ sha256sum or shasum is required for rootfs cache identity" >&2
		exit 1
	fi
}

rootfs_namespace() {
	printf '%s/%s\n' "$ROOTFS_CACHE_VERSION" "$(sha256_text "$1")"
}

if [ -n "$PRINT_ROOTFS_NAMESPACE" ]; then
	rootfs_namespace "$PRINT_ROOTFS_NAMESPACE"
	exit 0
fi

need() { command -v "$1" >/dev/null 2>&1 || { echo "✗ missing dependency: $1" >&2; exit 1; }; }
need docker

# Current pinned value of an ARG (e.g. `cur CLAUDE_VERSION` → 2.1.185).
cur() { grep -E "^ARG ${1}=" "$DOCKERFILE" | head -1 | sed -E "s/^ARG ${1}=//"; }
# Rewrite an ARG pin in place. Versions are alnum/dot/dash only — safe in s///.
set_arg() { perl -i -pe "s/^ARG ${1}=.*\$/ARG ${1}=${2}/" "$DOCKERFILE"; }

if [ "$DO_UPDATE" = 1 ]; then
	need npm; need gh; need jq; need curl
	# claude installs via its native installer, but its versions match the
	# @anthropic-ai/claude-code npm package. codex tracks the latest *stable*
	# (non-prerelease) github release (rust-v<ver> tag). cursor's official
	# installer pins the current lab artifact. amp/opencode/pi take the npm
	# `latest` dist-tag; Prime uses its official installer's stable feed.
	CLAUDE_NEW=$(npm view @anthropic-ai/claude-code version)
	CODEX_NEW=$(gh api repos/openai/codex/releases/latest --jq .tag_name | sed 's/^rust-v//')
	CURSOR_NEW=$(curl -fsSL https://cursor.com/install \
		| sed -nE 's#^DOWNLOAD_URL="https://downloads\.cursor\.com/lab/([^/]+)/\$\{OS\}/\$\{ARCH\}/agent-cli-package\.tar\.gz"$#\1#p' \
		| head -1)
	OPENCODE_NEW=$(npm view opencode-ai version)
	PI_NEW=$(npm view @earendil-works/pi-coding-agent version)
	PRIME_AGENT_NEW=$(curl --proto '=https' --proto-redir '=https' -fsSL \
		https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev/stable | tr -d '[:space:]' | sed 's/^v//')
	AMP_NEW=$(npm view @ampcode/cli version)

	changed=0
	printf '  %-13s %-28s   %s\n' agent current latest
	for row in \
		"CLAUDE_VERSION:$CLAUDE_NEW" \
		"CODEX_VERSION:$CODEX_NEW" \
		"CURSOR_AGENT_VERSION:$CURSOR_NEW" \
		"OPENCODE_VERSION:$OPENCODE_NEW" \
		"PI_VERSION:$PI_NEW" \
		"PRIME_AGENT_VERSION:$PRIME_AGENT_NEW" \
		"AMP_VERSION:$AMP_NEW"; do
		name=${row%%:*}; new=${row#*:}
		[[ "$new" =~ ^[0-9][0-9A-Za-z.-]*$ ]] || { echo "✗ invalid or empty version for $name: $new" >&2; exit 1; }
		old=$(cur "$name"); mark=""
		[ "$old" != "$new" ] && { mark="  ←"; changed=1; }
		printf '  %-13s %-28s → %s%s\n' "${name%_VERSION}" "$old" "$new" "$mark"
		[ "$DRY_RUN" = 1 ] || set_arg "$name" "$new"
	done

	if [ "$DRY_RUN" = 1 ]; then
		echo "(dry run — runner/Dockerfile not modified)"; exit 0
	fi
	[ "$changed" = 1 ] \
		&& echo "→ bumped runner/Dockerfile pins; review \`git diff $DOCKERFILE\` and commit" \
		|| echo "→ all pins already latest — runner/Dockerfile unchanged"
fi

echo "▶ building $TAG (native arch, layer-cached)…"
args=(buildx build -f "$DOCKERFILE" -t "$TAG" --load)
[ "$NO_CACHE" = 1 ] && args+=(--no-cache)
args+=(.)
docker "${args[@]}"

bash scripts/verify-runner.sh "$TAG" "$(cur CODEX_VERSION)"

echo "✓ $TAG ready"
