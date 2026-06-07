#!/usr/bin/env bash
# Run on the VPS via: bash ~/dojjo-build/build.sh (remote BUILD_STRATEGY only)
set -euo pipefail
cd "$(dirname "$0")"
# shellcheck disable=SC1091
source "$HOME/.cargo/env"
./scripts/init-sync-server-db.sh
export DATABASE_URL="sqlite://${PWD}/sync-server/data.db"
echo "cargo build --release -p sync-server (first run can take several minutes)..."
cargo build --release -p sync-server
test -f target/release/sync-server
