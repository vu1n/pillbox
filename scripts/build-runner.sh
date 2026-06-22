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
TAG=pillbox-runner:l7   # the tag the ghost/ace/eval/dispatch/dogfood stack defaults to
DO_UPDATE=0 DRY_RUN=0 NO_CACHE=0 PRUNE=0

usage() {
	cat <<'EOF'
Usage: scripts/build-runner.sh [options]

  (no options)       rebuild the current pins (layer-cached) and verify
  -u, --update       resolve each agent's latest version, rewrite the pins in
                     runner/Dockerfile, rebuild, verify
      --dry-run      with --update: print what would change, don't write or build
  -t, --tag TAG      image tag to build (default: pillbox-runner:l7)
      --no-cache     force a clean rebuild (pass --no-cache to docker)
      --prune-rootfs after build, drop stale libkrun rootfs generations for this
                     tag (run only when no sessions are using the old image)
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
		-t|--tag)        TAG="${2:?--tag needs a value}"; shift ;;
		-h|--help)       usage; exit 0 ;;
		*) echo "✗ unknown arg: $1" >&2; usage >&2; exit 2 ;;
	esac
	shift
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "✗ missing dependency: $1" >&2; exit 1; }; }
need docker

# Current pinned value of an ARG (e.g. `cur CLAUDE_VERSION` → 2.1.185).
cur() { grep -E "^ARG ${1}=" "$DOCKERFILE" | head -1 | sed -E "s/^ARG ${1}=//"; }
# Rewrite an ARG pin in place. Versions are alnum/dot/dash only — safe in s///.
set_arg() { perl -i -pe "s/^ARG ${1}=.*\$/ARG ${1}=${2}/" "$DOCKERFILE"; }

if [ "$DO_UPDATE" = 1 ]; then
	need npm; need gh; need jq
	# claude installs via its native installer, but its versions match the
	# @anthropic-ai/claude-code npm package. codex tracks the latest *stable*
	# (non-prerelease) github release (rust-v<ver> tag). amp/opencode/pi take the
	# npm `latest` dist-tag.
	CLAUDE_NEW=$(npm view @anthropic-ai/claude-code version)
	CODEX_NEW=$(gh api repos/openai/codex/releases \
		--jq '[.[] | select(.prerelease==false and .draft==false)][0].tag_name' | sed 's/^rust-v//')
	OPENCODE_NEW=$(npm view opencode-ai version)
	PI_NEW=$(npm view @earendil-works/pi-coding-agent version)
	AMP_NEW=$(npm view @ampcode/cli version)

	changed=0
	printf '  %-9s %-28s   %s\n' agent current latest
	for row in \
		"CLAUDE_VERSION:$CLAUDE_NEW" \
		"CODEX_VERSION:$CODEX_NEW" \
		"OPENCODE_VERSION:$OPENCODE_NEW" \
		"PI_VERSION:$PI_NEW" \
		"AMP_VERSION:$AMP_NEW"; do
		name=${row%%:*}; new=${row#*:}
		[ -n "$new" ] || { echo "✗ could not resolve latest for $name" >&2; exit 1; }
		old=$(cur "$name"); mark=""
		[ "$old" != "$new" ] && { mark="  ←"; changed=1; }
		printf '  %-9s %-28s → %s%s\n' "${name%_VERSION}" "$old" "$new" "$mark"
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

echo "▶ agent versions baked into $TAG:"
docker run --rm --entrypoint sh "$TAG" -c '
	for a in claude codex amp opencode pi pillbox; do
		printf "  %-9s %s\n" "$a" "$($a --version 2>&1 | head -1)"
	done
'

# Each rebuild gives the image a new id, so libkrun re-materializes its rootfs
# (~/.pillbox/krun/rootfs/<sanitized-tag>_<sanitized-id>/) on the next run and
# the prior generation lingers. Opt-in prune drops generations for THIS tag that
# don't match the freshly-built id — never the current one, never other tags.
if [ "$PRUNE" = 1 ]; then
	root="${HOME}/.pillbox/krun/rootfs"
	if [ -d "$root" ]; then
		san() { printf '%s' "$1" | sed 's/[^a-zA-Z0-9]/_/g'; }   # mirrors Rust sanitize()
		new_id=$(docker image inspect "$TAG" --format '{{.Id}}')
		keep="$(san "$TAG")_$(san "$new_id")"
		pruned=0
		for d in "$root/$(san "$TAG")_sha256_"*; do
			[ -d "$d" ] || continue
			[ "$(basename "$d")" = "$keep" ] && continue
			rm -rf "$d" && pruned=$((pruned + 1))
		done
		echo "▶ pruned $pruned stale rootfs generation(s) for $TAG"
	fi
fi

echo "✓ $TAG ready"
