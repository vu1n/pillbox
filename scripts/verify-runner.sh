#!/usr/bin/env bash
# Verify the complete pinned package and all bundled CLIs without model calls.
set -euo pipefail
IMAGE=${1:?usage: verify-runner.sh IMAGE CODEX_VERSION}
CODEX_VERSION_PIN=${2:?expected Codex version required}
echo "▶ verifying the pinned Codex native package in $IMAGE:"
docker run --rm --entrypoint sh -e EXPECTED_CODEX_VERSION="$CODEX_VERSION_PIN" "$IMAGE" -c '
	set -eu
	package_root="${CODEX_PACKAGE_ROOT:?CODEX_PACKAGE_ROOT is not set}"
	expected_version="${EXPECTED_CODEX_VERSION:?EXPECTED_CODEX_VERSION is not set}"
	manifest="$package_root/codex-package.json"
	test -f "$manifest"
	test "$(jq -er ".version | select(type == \"string\")" "$manifest")" = "$expected_version"
	test "$(jq -er ".layoutVersion == 1" "$manifest")" = true
	test "$(jq -er ".entrypoint == \"bin/codex\"" "$manifest")" = true
	test "$(jq -er ".resourcesDir == \"codex-resources\"" "$manifest")" = true
	test "$(jq -er ".pathDir == \"codex-path\"" "$manifest")" = true
	test -x "$package_root/bin/codex"
	test -x "$package_root/bin/codex-code-mode-host"
	test -x "$package_root/codex-resources/zsh/bin/zsh"
	test -x "$package_root/codex-path/rg"
	test -L /usr/local/bin/codex
	test -L /usr/local/bin/codex-code-mode-host
	test "$(readlink -f /usr/local/bin/codex)" = "$package_root/bin/codex"
	test "$(readlink -f /usr/local/bin/codex-code-mode-host)" = "$package_root/bin/codex-code-mode-host"
	echo "  codex package: $expected_version + code-mode host + resources + path tools ✓"
'

echo "▶ agent versions baked into $IMAGE:"
docker run --rm --entrypoint sh "$IMAGE" -c '
	set -eu
	for a in claude codex amp opencode pi prime-agent agent pillbox; do
		version=$("$a" --version 2>&1) || { printf "%s version probe failed: %s\n" "$a" "$version" >&2; exit 1; }
		printf "  %-12s %s\n" "$a" "$(printf "%s\n" "$version" | head -1)"
	done
'

