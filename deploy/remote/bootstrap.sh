#!/usr/bin/env bash
# Run on the VPS via: sudo bash /tmp/dojjo-bootstrap.sh <install_bin> <data_dir>
set -euo pipefail
install_bin="${1:?install_bin}"
data_dir="${2:?data_dir}"

if ! id -u dojjo >/dev/null 2>&1; then
  useradd --system --home "$data_dir" --shell /usr/sbin/nologin dojjo
fi

mkdir -p "$install_bin" "$data_dir"
chown dojjo:dojjo "$data_dir"
chmod 755 "$install_bin"
chmod 750 "$data_dir"

if command -v apt-get >/dev/null 2>&1; then
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq git ca-certificates libssl3 2>/dev/null \
    || DEBIAN_FRONTEND=noninteractive apt-get install -y -qq git ca-certificates libssl1.1
fi
