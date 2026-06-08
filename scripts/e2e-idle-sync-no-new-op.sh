#!/usr/bin/env bash
# Probe idle dev sync: no jj commits, same jj op log id, measure push/pull churn.
# Passes if op id stays stable; reports whether repeat syncs are quiet (goal after fixes).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
"${ROOT}/scripts/init-sync-server-db.sh"
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

e2e_start_server "$TMP" "-idle-sync"
e2e_init_dojjo_clients

ORIGIN="${TMP}/origin"
CLONE="${TMP}/clone"
NEUTRAL="${TMP}/neutral"
mkdir -p "$ORIGIN" "$NEUTRAL"

(cd "$ORIGIN" && "$JJ" git init --no-colocate)
echo "hello" > "${ORIGIN}/README.md"
(cd "$ORIGIN" && "$JJ" file track README.md && "$JJ" commit -m "init")

OUT="$(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" create 2>&1)"
DOJO_ID="$(echo "$OUT" | sed -n 's/.*created dojo \([^ ;]*\).*/\1/p')"
[[ -n "$DOJO_ID" ]] || { echo "no dojo id" >&2; exit 1; }

DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_A" "$DOJO_ID")"
rm -rf "$CLONE"
mkdir -p "$CLONE"
e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "clone" "$NEUTRAL"
DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_B" "$DOJO_ID")"
e2e_assert_distinct_paths "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B"

# Converge after create/join; no jj commits from here.
for _ in 1 2 3; do
  (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null)
  (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null)
done

echo "baseline operation id (peer A): $(e2e_jj_current_operation_id "$ORIGIN" | head -c 16)…"
echo "baseline operation id (peer B): $(e2e_jj_current_operation_id "$CLONE" | head -c 16)…"
echo "local repo op_heads files (A): $(find "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A/op_heads/heads" -type f 2>/dev/null | wc -l | tr -d ' ')"
echo "local repo op_heads files (B): $(find "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B/op_heads/heads" -type f 2>/dev/null | wc -l | tr -d ' ')"

LAST_PUSH=0
LAST_PULL=0
A2_PUSH=-1
A2_PULL=-1
A4_PUSH=-1
A4_PULL=-1

run_step() {
  local label="$1"
  local home="$2"
  local ws="$3"
  local jj_repo_folder="$4"

  local op_before op_after idx_before idx_after
  op_before="$(e2e_jj_current_operation_id "$ws")"
  idx_before="$(e2e_mirror_sha256_index "$jj_repo_folder")"
  local diff_before
  diff_before="$(e2e_mirror_local_vs_server_diff "$jj_repo_folder" "$DOJJO_API_BASE" "$DOJO_ID" | head -20 || true)"

  local counts push pull
  counts="$(e2e_sync_count_push_pull "$home" "$ws")"
  read -r push pull <<<"$counts"
  LAST_PUSH=$push
  LAST_PULL=$pull

  op_after="$(e2e_jj_current_operation_id "$ws")"
  idx_after="$(e2e_mirror_sha256_index "$jj_repo_folder")"
  local changed_on_disk
  changed_on_disk="$(e2e_mirror_index_changed_paths "$idx_before" "$idx_after" | head -20 || true)"

  echo ""
  echo "=== $label ==="
  echo "  op id before: ${op_before:0:16}…  after: ${op_after:0:16}…"
  if [[ "$op_before" != "$op_after" ]]; then
    echo "  ERROR: operation id changed without an intentional jj commit in this test" >&2
    exit 1
  fi
  echo "  sync: push=$push pull=$pull"
  if [[ "$push" != "0" || "$pull" != "0" ]]; then
    echo "  local≠server before sync (first 20 paths):"
    if [[ -n "$diff_before" ]]; then
      echo "$diff_before" | sed 's/^/    /'
    else
      echo "    (none — diff uses pre-sync index; see on-disk changes below)"
    fi
    echo "  on-disk paths changed during sync (first 20):"
    if [[ -n "$changed_on_disk" ]]; then
      echo "$changed_on_disk" | sed 's/^/    /'
    else
      echo "    (none)"
    fi
  fi
}

# User-reported sequence: A, A, B, B, A (no jj edits).
run_step "A sync #1" "$DOJJO_HOME_A" "$ORIGIN" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A"
run_step "A sync #2" "$DOJJO_HOME_A" "$ORIGIN" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A"
A2_PUSH=$LAST_PUSH
A2_PULL=$LAST_PULL
run_step "B sync #1" "$DOJJO_HOME_B" "$CLONE" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B"
run_step "B sync #2" "$DOJJO_HOME_B" "$CLONE" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B"
run_step "A sync #3 (after B)" "$DOJJO_HOME_A" "$ORIGIN" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A"

run_step "A sync #4 (immediate repeat)" "$DOJJO_HOME_A" "$ORIGIN" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A"
A4_PUSH=$LAST_PUSH
A4_PULL=$LAST_PULL

echo ""
if [[ "$A2_PUSH" == "0" && "$A2_PULL" == "0" && "$A4_PUSH" == "0" && "$A4_PULL" == "0" ]]; then
  echo "idle-sync probe: repeat A syncs are quiet (push/pull churn fixed for this scenario)"
else
  echo "idle-sync probe: repeat A syncs still churn (A#2 push=$A2_PUSH pull=$A2_PULL; A#4 push=$A4_PUSH pull=$A4_PULL)"
  echo "  typical diff paths: op_heads/heads/* (not current op), store/extra/heads/*"
  if [[ "${E2E_IDLE_SYNC_EXPECT_QUIET:-1}" == "1" ]]; then
    exit 1
  fi
fi
echo "idle-sync-no-new-op e2e OK (op id stable; see churn line above)"
