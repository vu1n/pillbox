#!/bin/sh
# Preserve the complete native Codex release before the installer scratch HOME
# is removed. The standalone binary finds its companion host and bundled tools
# relative to this package layout, so copying only bin/codex is incomplete.
set -eu

die() {
	printf 'codex package: %s\n' "$1" >&2
	exit 1
}

usage() {
	cat >&2 <<'EOF'
usage: install-codex-package.sh SCRATCH_HOME VERSION [INSTALL_ROOT] [BIN_DIR]

Copy the native Codex package selected by SCRATCH_HOME/.local/bin/codex into
INSTALL_ROOT/packages/standalone/releases/VERSION and link codex plus its
code-mode host into BIN_DIR.
EOF
	exit 2
}

[ "$#" -ge 2 ] && [ "$#" -le 4 ] || usage

scratch_home=$1
expected_version=$2
install_root=${3:-/opt/codex}
bin_dir=${4:-/usr/local/bin}

[ -d "$scratch_home" ] || die "scratch HOME does not exist: $scratch_home"
[ -n "$expected_version" ] || die "expected Codex version is empty"

launcher="$scratch_home/.local/bin/codex"
[ -L "$launcher" ] || die "installer launcher is not a symlink: $launcher"

resolve_symlinks() {
	path=$1
	depth=0
	while [ -L "$path" ]; do
		depth=$((depth + 1))
		[ "$depth" -le 32 ] || die "symlink chain is too deep: $path"
		target=$(readlink "$path") || die "cannot read symlink: $path"
		case "$target" in
			/*) path=$target ;;
			*) path="$(dirname "$path")/$target" ;;
		esac
	done
	path_dir=$(CDPATH=; cd -P -- "$(dirname "$path")" 2>/dev/null && pwd -P) ||
		die "symlink target directory does not exist: $path"
	printf '%s/%s\n' "$path_dir" "$(basename "$path")"
}

resolved_entrypoint=$(resolve_symlinks "$launcher")
[ -x "$resolved_entrypoint" ] || die "launcher target is not executable: $resolved_entrypoint"

package_root=$(dirname "$resolved_entrypoint")
while :; do
	if [ -f "$package_root/codex-package.json" ]; then
		break
	fi
	next_root=$(dirname "$package_root")
	[ "$next_root" != "$package_root" ] || die "package manifest not found above $resolved_entrypoint"
	package_root=$next_root
done

package_root=$(CDPATH=; cd -P -- "$package_root" 2>/dev/null && pwd -P) ||
	die "package root does not exist"
release_root=$(CDPATH=; cd -P -- "$scratch_home/.codex/packages/standalone/releases" 2>/dev/null && pwd -P) ||
	die "standalone release directory does not exist under scratch HOME"
case "$package_root" in
	"$release_root"/*) ;;
	*) die "launcher resolves outside standalone releases: $package_root" ;;
esac

manifest="$package_root/codex-package.json"
manifest_field() {
	jq -er "$1" "$manifest" 2>/dev/null
}

[ "$(manifest_field '.layoutVersion == 1')" = true ] ||
	die "manifest layoutVersion is not 1: $manifest"
manifest_version=$(manifest_field '.version | select(type == "string")') ||
	die "manifest version is missing: $manifest"
[ "$manifest_version" = "$expected_version" ] ||
	die "manifest version $manifest_version does not match expected $expected_version"
[ "$(manifest_field '.variant == "codex"')" = true ] ||
	die "manifest variant is not codex: $manifest"
manifest_target=$(manifest_field '.target | select(type == "string")') ||
	die "manifest target is missing: $manifest"
case "$manifest_target" in
	*-linux-*) ;;
	*) die "manifest target is not a Linux release: $manifest_target" ;;
esac
[ "$(manifest_field '.entrypoint == "bin/codex"')" = true ] ||
	die "manifest entrypoint is not bin/codex: $manifest"
resources_dir=$(manifest_field '.resourcesDir | select(type == "string" and length > 0)') ||
	die "manifest resourcesDir is missing: $manifest"
path_dir=$(manifest_field '.pathDir | select(type == "string" and length > 0)') ||
	die "manifest pathDir is missing: $manifest"
case "$resources_dir" in
	/*|*..*) die "manifest resourcesDir is not a safe relative path: $resources_dir" ;;
esac
case "$path_dir" in
	/*|*..*) die "manifest pathDir is not a safe relative path: $path_dir" ;;
esac

entrypoint="$package_root/bin/codex"
helper="$package_root/bin/codex-code-mode-host"
resources="$package_root/$resources_dir"
path_tools="$package_root/$path_dir"
[ -x "$entrypoint" ] || die "manifest entrypoint is missing or not executable: $entrypoint"
[ -x "$helper" ] || die "code-mode host is missing or not executable: $helper"
[ -d "$resources/zsh/bin" ] || die "bundled zsh resources are missing: $resources/zsh/bin"
[ -x "$resources/zsh/bin/zsh" ] || die "bundled zsh is missing or not executable: $resources/zsh/bin/zsh"
[ -d "$path_tools" ] || die "bundled path tools are missing: $path_tools"
[ -x "$path_tools/rg" ] || die "bundled ripgrep is missing or not executable: $path_tools/rg"

stable_package="$install_root/packages/standalone/releases/$expected_version"
[ ! -e "$stable_package" ] && [ ! -L "$stable_package" ] ||
	die "stable package destination already exists: $stable_package"
mkdir -p "$(dirname "$stable_package")"
mkdir "$stable_package"
cp -a "$package_root"/. "$stable_package"/

mkdir -p "$bin_dir"
for name in codex codex-code-mode-host; do
	link="$bin_dir/$name"
	[ ! -e "$link" ] && [ ! -L "$link" ] || die "PATH link already exists: $link"
	ln -s "$stable_package/bin/$name" "$link"
done

[ -L "$bin_dir/codex" ] || die "Codex PATH link was not created"
[ -L "$bin_dir/codex-code-mode-host" ] || die "code-mode host PATH link was not created"
[ -x "$bin_dir/codex" ] || die "Codex PATH link is not executable"
[ -x "$bin_dir/codex-code-mode-host" ] || die "code-mode host PATH link is not executable"

printf 'codex package: preserved %s at %s\n' "$expected_version" "$stable_package"
