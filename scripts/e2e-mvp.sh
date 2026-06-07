#!/usr/bin/env bash
# MVP smoke: sync-server + dojjo create / cold join / dev sync with two simulated clients
# (DOJJO_HOME_A / DOJJO_HOME_B). Distinct physical sync-host repos; convergence via server only.
# See README.md and docs/DEVELOPMENT.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
"${ROOT}/scripts/init-sync-server-db.sh"
cargo build -q --bin dojjo --bin sync-server

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
SYNC_SERVER="${TARGET_DIR}/debug/sync-server"
DOJJO="${TARGET_DIR}/debug/dojjo"
JJ="$(command -v jj || true)"

if [[ ! -x "$SYNC_SERVER" ]] || [[ ! -x "$DOJJO" ]]; then
  echo "missing binaries (expected $SYNC_SERVER and $DOJJO); run: cargo build --bin dojjo --bin sync-server" >&2
  exit 1
fi
if [[ -z "$JJ" ]]; then
  echo "jj not found on PATH; install Jujutsu 0.38.x to run this script" >&2
  exit 1
fi

GIT="${GIT:-$(command -v git)}"
if [[ -z "$GIT" ]]; then
  echo "git not found on PATH" >&2
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

e2e_start_server "$TMP" "-mvp"
e2e_init_dojjo_clients

ORIGIN="${TMP}/origin"
CLONE="${TMP}/clone"
NEUTRAL="${TMP}/neutral"
mkdir -p "$ORIGIN" "$NEUTRAL"

(cd "$ORIGIN" && "$JJ" git init --no-colocate)
echo "hello" > "${ORIGIN}/README.md"
(cd "$ORIGIN" && "$JJ" file track README.md && "$JJ" commit -m "init")

OUT="$(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" create 2>&1)"
echo "$OUT"
DOJO_ID="$(echo "$OUT" | sed -n 's/.*created dojo \([^ ;]*\).*/\1/p')"
if [[ -z "$DOJO_ID" ]]; then
  echo "failed to parse dojo id from dojjo create" >&2
  exit 1
fi

SYNC_REPO_A="$(e2e_physical_sync_repo "$DOJJO_HOME_A" "$DOJO_ID")"
if [[ ! -d "$SYNC_REPO_A/store" ]]; then
  echo "expected physical sync repo at $SYNC_REPO_A" >&2
  exit 1
fi
if [[ ! -f "${ORIGIN}/.jj/repo" ]]; then
  echo "origin .jj/repo must be a pointer file after create (got $(ls -la "${ORIGIN}/.jj/repo" 2>&1))" >&2
  exit 1
fi

rm -rf "$CLONE"
mkdir -p "$CLONE"
e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "clone" "$NEUTRAL"

SYNC_REPO_B="$(e2e_physical_sync_repo "$DOJJO_HOME_B" "$DOJO_ID")"
e2e_assert_distinct_paths "$SYNC_REPO_A" "$SYNC_REPO_B"

echo "--- jj workspace list (origin) ---"
(cd "$ORIGIN" && "$JJ" workspace list)

echo "--- jj log (origin) ---"
(cd "$ORIGIN" && "$JJ" log -n 3)

echo "--- jj log (clone) ---"
(cd "$CLONE" && "$JJ" log -n 3)

echo "--- edit origin, sync both ---"
e2e_stop_all_background_sync
echo "from-origin" >> "${ORIGIN}/README.md"
(cd "$ORIGIN" && "$JJ" commit -m "origin edit")
ORIGIN_EDIT_ID="$(e2e_jj_commit_id_at "$ORIGIN" '@-')"
if [[ -z "$ORIGIN_EDIT_ID" ]]; then
  echo "failed to read origin edit commit id" >&2
  exit 1
fi
e2e_assert_clone_lacks_commit "$CLONE" "$ORIGIN_EDIT_ID"

(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync)
e2e_assert_clone_lacks_commit "$CLONE" "$ORIGIN_EDIT_ID"

(cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync)
(cd "$CLONE" && "$JJ" workspace update-stale)

if ! (cd "$CLONE" && "$JJ" show -r "$ORIGIN_EDIT_ID" >/dev/null 2>&1); then
  echo "clone must see origin edit ($ORIGIN_EDIT_ID) after sync" >&2
  exit 1
fi

e2e_assert_peers_converged "$ORIGIN" "$CLONE" "" "$SYNC_REPO_A" "$SYNC_REPO_B"

echo "--- jj log after sync (origin) ---"
(cd "$ORIGIN" && "$JJ" log -n 5)

echo "--- jj log after sync (clone) ---"
(cd "$CLONE" && "$JJ" log -n 5)

echo "--- clone status ---"
(cd "$CLONE" && "$JJ" st)

echo "e2e OK (two clients)"
