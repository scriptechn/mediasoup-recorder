#!/usr/bin/env bash
# Every check this crate has to pass, in one command, so a laptop and CI run the same list.
# Needs GStreamer development libraries (Linux); on Windows/macOS without them, run it through Docker:
#   docker build --target build -t recorder-check . && docker run --rm recorder-check ./scripts/check.sh
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt"
cargo fmt --all -- --check

echo "==> cargo clippy"
cargo clippy --all-targets --all-features -- -D warnings

echo "==> cargo test"
cargo test --all
