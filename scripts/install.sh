#!/usr/bin/env bash
# Install a STABLE pillbox binary, isolated from the build dir.
#
# Why: target/{debug,release}/pillbox is build OUTPUT — every `cargo build` /
# `cargo test` / CI rewrites it (and during a build it's momentarily half-written),
# so it's a terrible daily driver. This copies a release build OUT to ~/.local/bin,
# where no cargo command touches it. Re-run to update the installed binary.
#
# libkrun (the default backend) is codesigned with the HVF entitlement so it can
# boot a VM on macOS — `cargo build` strips the signature, so we (re)sign the
# installed copy. macOS only; on Linux/KVM no codesign is needed (the codesign
# step is a no-op-or-skip there).
set -euo pipefail
cd "$(dirname "$0")/.."

DEST="${PILLBOX_INSTALL_DIR:-$HOME/.local/bin}/pillbox"
ENTITLEMENTS="krun/entitlements.plist"

echo "pillbox: building release (libkrun is the default backend)…"
cargo build --release

install -d "$(dirname "$DEST")"
install -m 755 target/release/pillbox "$DEST"

if [ "$(uname)" = "Darwin" ]; then
  codesign -f --entitlements "$ENTITLEMENTS" -s - "$DEST"
  echo "pillbox: codesigned for libkrun/HVF"
fi

echo "pillbox: installed → $DEST"
command -v pillbox >/dev/null && echo "pillbox: on PATH ✓" || \
  echo "pillbox: NOTE $(dirname "$DEST") not on PATH — add it"
"$DEST" version 2>/dev/null || true
