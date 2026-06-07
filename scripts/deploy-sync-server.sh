#!/usr/bin/env bash
# Build sync-server and install on a Tailscale VPS (systemd).
#
# Default build strategy (BUILD_STRATEGY=auto):
#   - Apple Silicon deploy host + x86_64 VPS → cargo-zigbuild on the deploy host (no Docker)
#   - All other auto cases → cross-rs in Docker on the deploy host
#
# Prerequisites (deploy host, zigbuild — auto on Apple Silicon → x86_64 Linux):
#   - cargo-zigbuild, zig on PATH, rustup target for the VPS triple
#
# Prerequisites (deploy host, cross strategy):
#   - cross from git + Docker (see check_cross_rs)
#
# Prerequisites (VPS):
#   - tailscaled, SSH (remote strategy also needs Rust on the VPS)
#
# Setup:
#   cp deploy/deploy.env.example deploy/deploy.env
#   $EDITOR deploy/deploy.env
#   ./scripts/deploy-sync-server.sh
#
# Usage:
#   ./scripts/deploy-sync-server.sh              # build + install + restart
#   ./scripts/deploy-sync-server.sh --build-only
#   ./scripts/deploy-sync-server.sh --install-only
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="${DOJJO_DEPLOY_ENV:-$ROOT/deploy/deploy.env}"
SYSTEMD_UNIT="$ROOT/deploy/systemd/dojjo-sync-server.service"
ARTIFACT_DIR="$ROOT/target/cross-artifacts"
REMOTE_BUILD_DIR="${DOJJO_REMOTE_BUILD_DIR:-dojjo-build}"
BUILD_ONLY=0
INSTALL_ONLY=0

for arg in "$@"; do
  case "$arg" in
    --build-only) BUILD_ONLY=1 ;;
    --install-only) INSTALL_ONLY=1 ;;
    -h|--help)
      sed -n '2,24p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $arg (try --help)" >&2
      exit 1
      ;;
  esac
done

if [[ ! -f "$ENV_FILE" ]]; then
  echo "missing $ENV_FILE — copy deploy/deploy.env.example and edit it" >&2
  exit 1
fi

# shellcheck source=/dev/null
source "$ENV_FILE"

: "${DEPLOY_USER:=$(whoami)}"
: "${DOJJO_INSTALL_BIN:=/opt/dojjo/bin}"
: "${DOJJO_DATA_DIR:=/var/lib/dojjo}"
: "${DATABASE_URL:=sqlite:///var/lib/dojjo/data.db}"
: "${BUILD_STRATEGY:=auto}"

# Pin VPS env before build — compile uses a local DATABASE_URL for sqlx::query! only.
SERVER_DATABASE_URL="$DATABASE_URL"
SERVER_DOJJO_DATA_DIR="$DOJJO_DATA_DIR"
SERVER_DOJJO_PUBLIC_URL="${DOJJO_PUBLIC_URL:-}"
SERVER_RUST_LOG="${RUST_LOG:-}"
SERVER_DOJJO_LISTEN="${DOJJO_LISTEN:-}"

if [[ -z "$SERVER_DOJJO_PUBLIC_URL" && -n "${DEPLOY_HOST:-}" ]]; then
  SERVER_DOJJO_PUBLIC_URL="http://${DEPLOY_HOST}:3000"
fi

SSH_BATCH=(ssh -o BatchMode=yes -o ConnectTimeout=15 "${DEPLOY_USER}@${DEPLOY_HOST}")
# -t allocates a TTY so remote `sudo` can prompt for a password when run from your terminal.
SSH_TTY=(ssh -t -o ConnectTimeout=15 "${DEPLOY_USER}@${DEPLOY_HOST}")
SCP=(scp -o BatchMode=yes -o ConnectTimeout=15)
REMOTE="${DEPLOY_USER}@${DEPLOY_HOST}"

remote_has_passwordless_sudo() {
  "${SSH_BATCH[@]}" 'sudo -n true' 2>/dev/null
}

# Run a remote shell command. Uses ssh -t when sudo needs a password (must run from a real terminal).
# Do not pipe/heredoc into this — copy scripts to the VPS and invoke them by path instead.
run_remote_cmd() {
  local remote_cmd="$1"
  if remote_has_passwordless_sudo; then
    "${SSH_BATCH[@]}" "$remote_cmd"
  elif [[ -t 1 ]]; then
    echo "note: enter your VPS sudo password when prompted" >&2
    "${SSH_TTY[@]}" "$remote_cmd"
  else
    echo "error: remote sudo needs a password but stdout is not a terminal." >&2
    echo "  Run ./scripts/deploy-sync-server.sh from an interactive terminal, or configure NOPASSWD sudo on the VPS." >&2
    exit 1
  fi
}

run_remote_sudo_script() {
  local local_script="$1"
  local remote_path="$2"
  shift 2
  local -a args=("$@")
  local remote_args=""
  local a
  for a in "${args[@]}"; do
    remote_args+=" $(printf '%q' "$a")"
  done
  "${SCP[@]}" "$local_script" "${REMOTE}:${remote_path}"
  run_remote_cmd "sudo bash ${remote_path}${remote_args}"
}

require_cmd() {
  local c="$1"
  command -v "$c" >/dev/null 2>&1 || {
    echo "missing command: $c ($2)" >&2
    exit 1
  }
}

detect_deploy_target() {
  if [[ -n "${DEPLOY_TARGET:-}" ]]; then
    echo "$DEPLOY_TARGET"
    return
  fi
  if [[ "$BUILD_ONLY" -eq 1 && "$BUILD_STRATEGY" != "remote" && -z "${DEPLOY_HOST:-}" ]]; then
    echo "DEPLOY_TARGET unset; defaulting to x86_64-unknown-linux-gnu (--build-only)" >&2
    echo x86_64-unknown-linux-gnu
    return
  fi
  : "${DEPLOY_HOST:?set DEPLOY_HOST in $ENV_FILE}"
  local arch
  arch="$("${SSH_BATCH[@]}" 'uname -m')"
  case "$arch" in
    x86_64) echo x86_64-unknown-linux-gnu ;;
    aarch64|arm64) echo aarch64-unknown-linux-gnu ;;
    *)
      echo "unsupported remote uname -m: $arch (set DEPLOY_TARGET in deploy.env)" >&2
      exit 1
      ;;
  esac
}

host_is_apple_silicon() {
  local m
  m="$(uname -m)"
  [[ "$m" == "arm64" || "$m" == "aarch64" ]]
}

choose_build_strategy() {
  local target="$1"
  case "$BUILD_STRATEGY" in
    cross|remote|zigbuild) echo "$BUILD_STRATEGY"; return ;;
    auto) ;;
    *)
      echo "BUILD_STRATEGY must be auto, zigbuild, cross, or remote (got $BUILD_STRATEGY)" >&2
      exit 1
      ;;
  esac
  if host_is_apple_silicon && [[ "$target" == "x86_64-unknown-linux-gnu" ]]; then
    echo "auto: Apple Silicon deploy host → x86_64 Linux uses cargo-zigbuild" >&2
    echo zigbuild
    return
  fi
  echo cross
}

ensure_zigbuild_target() {
  local target="$1"
  require_cmd cargo-zigbuild "cargo install --locked cargo-zigbuild"
  require_cmd zig "install zig (mise, your package manager, https://ziglang.org/download, …)"
  if ! rustup target list --installed | grep -qx "$target"; then
    echo "installing rustup target $target..."
    rustup target add "$target"
  fi
}

build_binary_zigbuild() {
  local target db_url
  target="$(detect_deploy_target)"
  ensure_zigbuild_target "$target"
  echo "build strategy: zigbuild (target $target)"

  "$ROOT/scripts/init-sync-server-db.sh"
  db_url="sqlite://$ROOT/sync-server/data.db"
  export DATABASE_URL="$db_url"

  cargo zigbuild --release -p sync-server --target "$target"

  mkdir -p "$ARTIFACT_DIR"
  cp "$ROOT/target/$target/release/sync-server" "$ARTIFACT_DIR/sync-server"
  echo "built $ARTIFACT_DIR/sync-server ($(file -b "$ARTIFACT_DIR/sync-server"))"
}

check_cross_rs() {
  require_cmd cross "cargo install cross --git https://github.com/cross-rs/cross"
  local ver
  ver="$(cross 2>&1 </dev/null | head -1 || true)"
  if [[ "$ver" != *"github.com/cross-rs/cross"* && "$ver" != *"cross-rs"* ]]; then
    if [[ "$ver" == "cross 0.2.5" && "$ver" != *"("* ]]; then
      echo "wrong cross: crates.io 0.2.5 breaks on Rust 1.92+ — reinstall:" >&2
      echo "  cargo install cross --git https://github.com/cross-rs/cross --force" >&2
      exit 1
    fi
  fi
}

check_docker() {
  if ! docker info >/dev/null 2>&1; then
    echo "Docker is not running — needed for BUILD_STRATEGY=cross (start the Docker engine on the deploy host)" >&2
    exit 1
  fi
}

rsync_sources() {
  require_cmd rsync "install rsync"
  : "${DEPLOY_HOST:?set DEPLOY_HOST in $ENV_FILE}"
  echo "syncing sources to ${DEPLOY_HOST}:~/${REMOTE_BUILD_DIR}/"
  rsync -az --delete \
    --exclude '.git/' \
    --exclude 'target/' \
    --exclude '.jj/' \
    --exclude 'jj-*/' \
    --exclude 'dojjo-data/' \
    --exclude 'deploy/deploy.env' \
    "$ROOT/" "${REMOTE}:~/${REMOTE_BUILD_DIR}/"
}

ensure_remote_build_deps() {
  "${SCP[@]}" "$ROOT/deploy/remote/build-deps.sh" "${REMOTE}:/tmp/dojjo-build-deps.sh"
  run_remote_cmd "bash /tmp/dojjo-build-deps.sh"
}

build_binary_cross() {
  local target db_url
  check_cross_rs
  check_docker
  target="$(detect_deploy_target)"
  echo "build strategy: cross (target $target)"

  "$ROOT/scripts/init-sync-server-db.sh"
  db_url="sqlite://$ROOT/sync-server/data.db"
  export DATABASE_URL="$db_url"

  cross build --release -p sync-server --target "$target"

  mkdir -p "$ARTIFACT_DIR"
  cp "$ROOT/target/$target/release/sync-server" "$ARTIFACT_DIR/sync-server"
  echo "built $ARTIFACT_DIR/sync-server"
}

build_binary_remote() {
  local target
  : "${DEPLOY_HOST:?set DEPLOY_HOST in $ENV_FILE}"
  target="$(detect_deploy_target)"
  echo "build strategy: remote on $DEPLOY_HOST (native $target)"

  rsync_sources
  ensure_remote_build_deps
  "${SCP[@]}" "$ROOT/deploy/remote/build.sh" "${REMOTE}:~/${REMOTE_BUILD_DIR}/deploy/remote/build.sh"
  run_remote_cmd "bash ~/${REMOTE_BUILD_DIR}/deploy/remote/build.sh"

  mkdir -p "$ARTIFACT_DIR"
  "${SCP[@]}" "${REMOTE}:~/${REMOTE_BUILD_DIR}/target/release/sync-server" "$ARTIFACT_DIR/sync-server"
  echo "built $ARTIFACT_DIR/sync-server"
}

build_binary() {
  local strategy target
  target="$(detect_deploy_target)"
  strategy="$(choose_build_strategy "$target")"
  case "$strategy" in
    zigbuild) build_binary_zigbuild ;;
    cross) build_binary_cross ;;
    remote) build_binary_remote ;;
    *)
      echo "internal error: unknown strategy $strategy" >&2
      exit 1
      ;;
  esac
}

resolve_dojjo_listen() {
  if [[ -n "$SERVER_DOJJO_LISTEN" ]]; then
    echo "$SERVER_DOJJO_LISTEN"
    return
  fi
  local ip
  ip="$("${SSH_BATCH[@]}" 'command -v tailscale >/dev/null && tailscale ip -4')"
  if [[ -z "$ip" ]]; then
    echo "DOJJO_LISTEN is unset and could not read tailscale ip -4 on $DEPLOY_HOST" >&2
    exit 1
  fi
  echo "${ip}:3000"
}

write_remote_env() {
  local listen="$1"
  local tmp
  tmp="$(mktemp)"

  {
    echo "DATABASE_URL=$SERVER_DATABASE_URL"
    echo "DOJJO_DATA_DIR=$SERVER_DOJJO_DATA_DIR"
    echo "DOJJO_LISTEN=$listen"
    if [[ -n "$SERVER_DOJJO_PUBLIC_URL" ]]; then
      echo "DOJJO_PUBLIC_URL=$SERVER_DOJJO_PUBLIC_URL"
    fi
    if [[ -n "$SERVER_RUST_LOG" ]]; then
      echo "RUST_LOG=$SERVER_RUST_LOG"
    fi
  } >"$tmp"

  "${SCP[@]}" "$tmp" "${REMOTE}:/tmp/dojjo-sync-server.env"
  run_remote_cmd 'sudo mkdir -p /etc/dojjo && sudo install -m 644 /tmp/dojjo-sync-server.env /etc/dojjo/sync-server.env'
  rm -f "$tmp"
}

bootstrap_remote() {
  run_remote_sudo_script "$ROOT/deploy/remote/bootstrap.sh" /tmp/dojjo-bootstrap.sh \
    "$DOJJO_INSTALL_BIN" "$DOJJO_DATA_DIR"
}

install_remote() {
  local listen
  listen="$(resolve_dojjo_listen)"

  if [[ ! -f "$ARTIFACT_DIR/sync-server" ]]; then
    echo "missing $ARTIFACT_DIR/sync-server — run without --install-only first" >&2
    exit 1
  fi

  bootstrap_remote
  write_remote_env "$listen"

  "${SCP[@]}" "$ARTIFACT_DIR/sync-server" "${REMOTE}:/tmp/sync-server.new"
  "${SCP[@]}" "$SYSTEMD_UNIT" "${REMOTE}:/tmp/dojjo-sync-server.service"

  run_remote_sudo_script "$ROOT/deploy/remote/install.sh" /tmp/dojjo-install.sh \
    "$DOJJO_INSTALL_BIN"

  echo "sync-server listening on http://${listen} (Tailscale only if DOJJO_LISTEN is the TS IP)"
  echo "client api_base: http://${DEPLOY_HOST}:3000/api"

  local i code=""
  # Routes live under /api and /git only — GET / returns 404 even when healthy.
  local probe="http://${listen}/api/dojo/__dojjo_deploy_probe__"
  for i in 1 2 3 4 5; do
    if code="$("${SSH_BATCH[@]}" "curl -sS -o /dev/null -w '%{http_code}' --connect-timeout 3 ${probe}" 2>/dev/null)" \
      && [[ "$code" =~ ^[0-9]+$ ]] \
      && [[ "$code" != "000" ]]; then
      echo "health: HTTP $code from ${probe} (404/405 expected — server is up)"
      return
    fi
    sleep 1
  done
  echo "warning: service not responding on http://${listen}/ — on the VPS run:" >&2
  echo "  sudo systemctl status dojjo-sync-server" >&2
  echo "  sudo journalctl -u dojjo-sync-server -n 30 --no-pager" >&2
  echo "  (if DATABASE_URL on the VPS points at a deploy-host path, re-run ./scripts/deploy-sync-server.sh --install-only)" >&2
}

if [[ "$BUILD_ONLY" -eq 1 && "$INSTALL_ONLY" -eq 1 ]]; then
  echo "choose at most one of --build-only and --install-only" >&2
  exit 1
fi

if [[ "$INSTALL_ONLY" -eq 0 ]]; then
  build_binary
fi

if [[ "$BUILD_ONLY" -eq 0 ]]; then
  install_remote
fi
