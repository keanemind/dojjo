#!/usr/bin/env bash
# Forensics for sync-safety scenario B2 (re-sync after torn pack).
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
echo "TMP=$TMP"

cleanup() {
  # keep tmp on failure for inspection
  e2e_stop_server 2>/dev/null || true
  echo "leaving TMP=$TMP"
}
trap cleanup EXIT

e2e_start_server "$TMP" "-debug-b2"
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

rm -rf "$CLONE"
mkdir -p "$CLONE"
e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "peer-b-B2" "$NEUTRAL"

DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_A" "$DOJO_ID")"
DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_B" "$DOJO_ID")"

origin_git="$(e2e_resolve_git_dir_from_repo "${ORIGIN}/.jj/repo")"
clone_git="$(e2e_resolve_git_dir_from_repo "${CLONE}/.jj/repo")"
bare="${DOJJO_DATA_DIR}/${DOJO_ID}/bare.git"

echo "origin_git=$origin_git"
echo "clone_git=$clone_git"
echo "bare=$bare"

for n in 1 2 3 4; do
  echo "x" >> "${ORIGIN}/README.md"
  (cd "$ORIGIN" && "$JJ" commit -m "r${n}" >/dev/null)
done

for _ in 1 2 3 4; do
  (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null)
  (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null)
done

echo "injecting torn pack into clone git store"
# same helper as e2e-sync-safety.sh
inject_torn_packfile() {
  local victim_git="$1"
  local donor_git="$2"
  chmod -R u+w "${victim_git}/objects" 2>/dev/null || true
  "$GIT" -C "$donor_git" repack -a -d >/dev/null 2>&1 || true
  local pack
  pack="$(find "${donor_git}/objects/pack" -name '*.pack' 2>/dev/null | head -1)" || return 0
  [[ -n "$pack" ]] || return 0
  local base size half
  base="$(basename "$pack")"
  mkdir -p "${victim_git}/objects/pack"
  size="$(wc -c <"$pack" | tr -d ' ')"
  half=$((size / 2))
  [[ "$half" -gt 0 ]] || half=1
  rm -f "${victim_git}/objects/pack/${base}" "${victim_git}/objects/pack/${base%.pack}.idx"
  head -c "$half" "$pack" >"${victim_git}/objects/pack/${base}"
  rm -f "${victim_git}/objects/pack/${base%.pack}.idx"
  find "${victim_git}/objects" -maxdepth 1 -type d -name '[0-9a-f][0-9a-f]' -exec rm -rf {} + 2>/dev/null || true
}
inject_torn_packfile "$clone_git" "$origin_git"

echo "running more sync rounds to heal"
for _ in 1 2 3 4 5 6; do
  (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null) || true
  (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null) || true
done

echo "== health probes (jj log, git fsck) =="
set +e
(cd "$CLONE" && "$JJ" log -n 8) >/dev/null 2>&1
jj_ok=$?
set -e
if [[ "$jj_ok" -ne 0 ]]; then
  echo "CLONE jj log FAILED"
  echo "git fsck clone:"
  "$GIT" -C "$clone_git" fsck --strict 2>&1 | head -40 | sed 's/^/  /' || true
  echo "git fsck bare:"
  "$GIT" -C "$bare" fsck --strict 2>&1 | head -40 | sed 's/^/  /' || true
  echo "jj keep refs on bare (first 20):"
  "$GIT" -C "$bare" for-each-ref --format='%(refname) %(objectname)' refs/jj/keep 2>/dev/null | head -20 | sed 's/^/  /' || true
  echo "jj keep refs on clone (first 20):"
  "$GIT" -C "$clone_git" for-each-ref --format='%(refname) %(objectname)' refs/jj/keep 2>/dev/null | head -20 | sed 's/^/  /' || true
  exit 1
fi

echo "CLONE jj log OK (healed)"
