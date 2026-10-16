#!/usr/bin/env bash
# Fast verification that runs on any host (macOS or Linux):
#   - unit and integration tests of every crate that does not open windows
#   - type-checking the UI and app (GPUI type-checks without a display)
#   - type-checking the Chromium backend for macOS without downloading CEF
#
# The full app only builds and runs on macOS: `cargo xtask bundle --run`.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> tests"
cargo test --quiet \
  -p cockpit-common -p cockpit-persistence -p cockpit-git -p cockpit-terminal \
  -p cockpit-editor -p cockpit-agents -p cockpit-orchestrator -p cockpit-browser \
  -p xtask

echo "==> UI and app (host)"
cargo check --quiet -p cockpit-ui -p cockpit --all-targets

echo "==> Chromium backend (macOS, type-check only)"
rustup target add aarch64-apple-darwin >/dev/null 2>&1 || true
cargo check --quiet -p cockpit-browser --features cef-typecheck --target aarch64-apple-darwin

echo "All checks passed."
