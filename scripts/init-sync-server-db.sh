#!/usr/bin/env bash
# Create or update sync-server/data.db so sqlx::query! can verify SQL at compile time.
# Requires sqlx-cli (`sqlx` on PATH, or `cargo sqlx`): cargo install sqlx-cli --no-default-features --features rustls,sqlite
# Run once per clone (and after pulling new migrations), before `cargo build -p sync-server`.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DB="${ROOT}/sync-server/data.db"
export DATABASE_URL="sqlite://${DB}"
MIGRATIONS="${ROOT}/sync-server/migrations"
if command -v sqlx >/dev/null 2>&1; then
  sqlx database create
  sqlx migrate run --source "${MIGRATIONS}"
else
  cargo sqlx database create
  cargo sqlx migrate run --source "${MIGRATIONS}"
fi
