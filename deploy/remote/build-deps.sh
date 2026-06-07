#!/usr/bin/env bash
# One-time VPS setup for BUILD_STRATEGY=remote (Rust + build packages). Run as SSH user.
set -euo pipefail
if command -v apt-get >/dev/null 2>&1; then
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    build-essential pkg-config libssl-dev git ca-certificates curl
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "installing Rust via rustup (one-time on VPS)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
cargo -V
