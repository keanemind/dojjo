#!/usr/bin/env bash
# Debug: after cold join pull, inspect sync_repo/store/git before git fetch.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
"${ROOT}/scripts/init-sync-server-db.sh"
export DATABASE_URL="sqlite://${ROOT}/sync-server/data.db"
cargo build -q --bin dojjo --bin sync-server

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
SYNC_SERVER="${TARGET_DIR}/debug/sync-server"
DOJJO="${TARGET_DIR}/debug/dojjo"
JJ="$(command -v jj || true)"
GIT="${GIT:-$(command -v git)}"

if [[ ! -x "$SYNC_SERVER" ]] || [[ ! -x "$DOJJO" ]] || [[ -z "$JJ" ]] || [[ -z "$GIT" ]]; then
  echo "need sync-server, dojjo, jj, git on PATH" >&2
  exit 1
fi

# shellcheck source=scripts/e2e-lib.sh
source "${ROOT}/scripts/e2e-lib.sh"

TMP="$(mktemp -d)"
e2e_init_peer_homes "$TMP"
cleanup() {
  e2e_stop_server
  rm -rf "$TMP"
}
trap cleanup EXIT

e2e_start_server "$TMP" "-debug-join-gitdir"
e2e_init_dojjo_clients

ORIGIN="${TMP}/origin"
CLONE="${TMP}/clone"
NEUTRAL="${TMP}/neutral"
mkdir -p "$ORIGIN" "$NEUTRAL" "$CLONE"

(cd "$ORIGIN" && "$JJ" git init --no-colocate)
echo "seed" > "${ORIGIN}/README.md"
(cd "$ORIGIN" && "$JJ" file track README.md && "$JJ" commit -m "init")

OUT="$(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" create 2>&1)"
DOJO_ID="$(echo "$OUT" | sed -n 's/.*created dojo \([^ ;]*\).*/\1/p')"
[[ -n "$DOJO_ID" ]] || { echo "no dojo id" >&2; exit 1; }

SYNC_REPO_B="$(e2e_physical_sync_repo "$DOJJO_HOME_B" "$DOJO_ID" || true)"
rm -rf "$CLONE"
mkdir -p "$CLONE"

echo "=== running cold join (expect it may fail) ==="
set +e
JOIN_OUT="$(e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "clone" "$NEUTRAL" 2>&1)"
JOIN_CODE=$?
set -e
echo "$JOIN_OUT"
echo "join exit=$JOIN_CODE"

SYNC_REPO_B="$(e2e_physical_sync_repo "$DOJJO_HOME_B" "$DOJO_ID" || true)"
echo "sync_repo_b=${SYNC_REPO_B:-<missing>}"
if [[ -n "${SYNC_REPO_B:-}" ]]; then
  echo "=== store/git_target ==="
  if [[ -f "${SYNC_REPO_B}/store/git_target" ]]; then
    cat "${SYNC_REPO_B}/store/git_target" || true
  else
    echo "(missing)"
  fi
  echo "=== store/git listing ==="
  ls -la "${SYNC_REPO_B}/store/git" 2>&1 | sed 's/^/  /' || true
  echo "=== store/git important files ==="
  for f in HEAD config packed-refs; do
    if [[ -f "${SYNC_REPO_B}/store/git/${f}" ]]; then
      echo "  ${f}: present"
    else
      echo "  ${f}: MISSING"
    fi
  done
  echo "=== store/git dirs ==="
  for d in objects refs; do
    if [[ -d "${SYNC_REPO_B}/store/git/${d}" ]]; then
      echo "  ${d}/: present"
    else
      echo "  ${d}/: MISSING"
    fi
  done
fi

