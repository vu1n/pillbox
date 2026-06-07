#!/usr/bin/env bash
# Build the libkrun-feature pillbox binary AND codesign it with the HVF
# entitlement — in ONE step, because a bare `cargo build`/`cargo test`/`clippy`
# strips the signature, after which libkrun can't create the VM and
# `select_backend()` silently falls back to docker (codex-serve then errors
# "libkrun backend only"). Run this before any libkrun run. macOS only.
#
#   Usage: scripts/lk-build.sh
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --features libkrun
codesign -f --entitlements krun/entitlements.plist -s - target/debug/pillbox
echo "pillbox: libkrun binary built + codesigned → target/debug/pillbox"
