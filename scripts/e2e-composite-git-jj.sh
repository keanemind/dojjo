#!/usr/bin/env bash
# Composite Git + JJ sync: two simulated clients (DOJJO_HOME_A / DOJJO_HOME_B), git transport
# Composite sync: Git HTTP for store/git/, JJ mirror for the rest of .jj/repo.
#
# Acceptance bar: distinct physical sync-host inodes; bridge-only visibility until B syncs;
# bidirectional convergence. Lighter smoke: scripts/e2e-mvp.sh (same harness).
# See README.md and docs/DEVELOPMENT.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
"${ROOT}/scripts/init-sync-server-db.sh"
cargo build -q --bin dojjo --bin sync-server

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
SYNC_SERVER="${TARGET_DIR}/debug/sync-server"
DOJJO="${TARGET_DIR}/debug/dojjo"
GIT="${GIT:-$(command -v git)}"
JJ="$(command -v jj || true)"

if [[ ! -x "$SYNC_SERVER" ]] || [[ ! -x "$DOJJO" ]]; then
  echo "missing dojjo or sync-server binaries" >&2
  exit 1
fi
if [[ -z "$JJ" ]]; then
  echo "jj not found on PATH" >&2
  exit 1
fi
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

run_two_peer_sync() {
  local label="$1"
  local init_cmd=("${@:2}")

  echo "=== composite e2e (two clients): ${label} ==="
  e2e_start_server "$TMP" "-${label}"
  e2e_init_dojjo_clients

  local ORIGIN="${TMP}/origin-${label}"
  local CLONE="${TMP}/clone-${label}"
  local PEER_A_NAME="peer-a-${label}"
  local PEER_B_NAME="peer-b-${label}"
  local NEUTRAL="${TMP}/neutral-${label}"
  rm -rf "$ORIGIN" "$CLONE" "$NEUTRAL"
  mkdir -p "$ORIGIN" "$NEUTRAL"

  (cd "$ORIGIN" && "${init_cmd[@]}")
  echo "seed" > "${ORIGIN}/README.md"
  (cd "$ORIGIN" && "$JJ" file track README.md && "$JJ" commit -m "init")

  OUT="$(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" create 2>&1)"
  echo "$OUT"
  DOJO_ID="$(echo "$OUT" | sed -n 's/.*created dojo \([^ ;]*\).*/\1/p')"
  if [[ -z "$DOJO_ID" ]]; then
    echo "failed to parse dojo id" >&2
    exit 1
  fi

  SYNC_REPO_A="$(e2e_physical_sync_repo "$DOJJO_HOME_A" "$DOJO_ID")"
  echo "peer A sync repo: $SYNC_REPO_A"

  GIT_REMOTE_URL="$(e2e_fetch_git_remote_url "$DOJJO_API_BASE" "$DOJO_ID")"
  echo "git remote: $GIT_REMOTE_URL"
  e2e_assert_git_http_ls_remote "$GIT_REMOTE_URL"

  e2e_assert_manifest_no_git_objects "$DOJJO_API_BASE" "$DOJO_ID"
  e2e_assert_manifest_no_workspace_store "$DOJJO_API_BASE" "$DOJO_ID"
  e2e_assert_tus_rejects_git_object_path "$DOJJO_API_BASE" "$DOJO_ID"

  rm -rf "$CLONE"
  mkdir -p "$CLONE"
  e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "$PEER_B_NAME" "$NEUTRAL"

  SYNC_REPO_B="$(e2e_physical_sync_repo "$DOJJO_HOME_B" "$DOJO_ID")"
  echo "peer B sync repo: $SYNC_REPO_B"
  e2e_assert_distinct_paths "$SYNC_REPO_A" "$SYNC_REPO_B"

  e2e_assert_jj_repo_usable "$ORIGIN"
  e2e_assert_jj_repo_usable "$CLONE"
  e2e_assert_git_dir_usable "$(e2e_resolve_git_dir_from_repo "$SYNC_REPO_A")"
  e2e_assert_git_dir_usable "$(e2e_resolve_git_dir_from_repo "$SYNC_REPO_B")"
  e2e_assert_manifest_no_git_objects "$DOJJO_API_BASE" "$DOJO_ID"
  e2e_assert_manifest_no_workspace_store "$DOJJO_API_BASE" "$DOJO_ID"

  echo "from-${label}" >> "${ORIGIN}/README.md"
  (cd "$ORIGIN" && "$JJ" commit -m "origin edit")
  ORIGIN_EDIT_ID="$(e2e_jj_commit_id_at "$ORIGIN" '@-')"
  if [[ -z "$ORIGIN_EDIT_ID" ]]; then
    echo "failed to read origin edit commit id" >&2
    exit 1
  fi

  e2e_assert_clone_lacks_commit "$CLONE" "$ORIGIN_EDIT_ID"

  (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync)
  e2e_assert_jj_repo_usable "$ORIGIN"
  e2e_assert_manifest_no_git_objects "$DOJJO_API_BASE" "$DOJO_ID"
  e2e_assert_manifest_no_workspace_store "$DOJJO_API_BASE" "$DOJO_ID"

  e2e_assert_clone_lacks_commit "$CLONE" "$ORIGIN_EDIT_ID"

  (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync)
  (cd "$CLONE" && "$JJ" workspace update-stale)
  e2e_assert_jj_repo_usable "$CLONE"
  e2e_assert_git_dir_usable "$(e2e_resolve_git_dir_from_repo "${CLONE}/.jj/repo")"

  if ! (cd "$CLONE" && "$JJ" show -r "$ORIGIN_EDIT_ID" >/dev/null 2>&1); then
    echo "clone must see origin edit ($ORIGIN_EDIT_ID) after sync" >&2
    exit 1
  fi

  e2e_assert_peers_converged "$ORIGIN" "$CLONE" "$label" "$SYNC_REPO_A" "$SYNC_REPO_B"

  echo "from-clone" >> "${CLONE}/README.md"
  (cd "$CLONE" && "$JJ" commit -m "clone edit")
  (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync)
  (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync)
  (cd "$ORIGIN" && "$JJ" workspace update-stale)

  e2e_assert_jj_repo_usable "$ORIGIN"
  e2e_assert_jj_repo_usable "$CLONE"
  e2e_assert_manifest_no_git_objects "$DOJJO_API_BASE" "$DOJO_ID"
  e2e_assert_manifest_no_workspace_store "$DOJJO_API_BASE" "$DOJO_ID"

  e2e_assert_peers_converged "$ORIGIN" "$CLONE" "$label" "$SYNC_REPO_A" "$SYNC_REPO_B"

  e2e_stop_server
  echo "=== ${label} OK ==="
}

# Git-backed dojos use internal store/git (git transport only; store/git_target is host-local).
run_two_peer_sync "git-backend" "$JJ" git init --no-colocate

echo "composite git+jj e2e OK (two clients)"
