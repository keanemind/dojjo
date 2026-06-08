#!/usr/bin/env bash
# Regression: cold join after two peer-A dev syncs must keep join-safe store/extra/heads markers.
#
# Natural flow (no manual server edits): peer A creates a dojo, commit + dev sync twice,
# then peer B cold-joins on a separate DOJJO_HOME. Historically, JJ multi-head merge during
# server sync-complete could leave segment blobs but zero head markers (see JJ_EXTRA_TABLE_HEADS_BUG.md).
# Server normalize now persists heads/<segment-id> after get_head().
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
  echo "need sync-server, dojjo, jj, git" >&2
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

e2e_start_server "$TMP" "-cold-join-extra-heads"
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
  echo "failed to parse dojo id" >&2
  exit 1
fi

MANIFEST_URL="${DOJJO_API_BASE}/dojo/${DOJO_ID}/mirror/manifest"

manifest_extra_stats() {
  curl -sf "$MANIFEST_URL" | python3 -c "
import json, sys
d = json.load(sys.stdin)
heads = sum(1 for e in d.get('entries', []) if e['path'].startswith('store/extra/heads/'))
segs = sum(
    1
    for e in d.get('entries', [])
    if e['path'].startswith('store/extra/')
    and not e['path'].startswith('store/extra/heads/')
)
print(f'{heads} {segs}')"
}

read -r heads_after_create segs_after_create < <(manifest_extra_stats)
echo "after create: store/extra/heads=$heads_after_create segments=$segs_after_create"
if [[ "$heads_after_create" -lt 1 ]]; then
  echo "expected create to publish at least one store/extra/heads marker" >&2
  exit 1
fi

echo "edit-1" >> "${ORIGIN}/README.md"
(cd "$ORIGIN" && "$JJ" commit -m "edit-1")
(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync)
read -r heads_after_sync1 segs_after_sync1 < <(manifest_extra_stats)
echo "after A sync #1: store/extra/heads=$heads_after_sync1 segments=$segs_after_sync1"

echo "edit-2" >> "${ORIGIN}/README.md"
(cd "$ORIGIN" && "$JJ" commit -m "edit-2")
(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync)
read -r heads_on_server segs_on_server < <(manifest_extra_stats)
echo "after A sync #2: store/extra/heads=$heads_on_server segments=$segs_on_server"

if [[ "$heads_on_server" -lt 1 ]]; then
  echo "expected server mirror to publish store/extra/heads after second A dev sync (got $heads_on_server)" >&2
  exit 1
fi
if [[ "$segs_on_server" -lt 1 ]]; then
  echo "expected store/extra segments on server manifest (got $segs_on_server)" >&2
  exit 1
fi

rm -rf "$CLONE" "${DOJJO_HOME_B}/dojos/${DOJO_ID}"
mkdir -p "$CLONE"
JOIN_OUT="$(cd "$NEUTRAL" && e2e_dojjo_for_home "$DOJJO_HOME_B" join \
  --dojo-id "$DOJO_ID" \
  --into "$CLONE" \
  --name "mac-lan-c" 2>&1)"
echo "$JOIN_OUT"

DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_B" "$DOJO_ID")"
if [[ ! -d "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B/store/extra/heads" ]]; then
  echo "peer B local repo must have store/extra/heads after cold join" >&2
  exit 1
fi
if ! compgen -G "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B/store/extra/heads/*" >/dev/null 2>&1; then
  echo "peer B local repo must have at least one extra head marker" >&2
  exit 1
fi

echo "e2e-cold-join-extra-heads: cold join ok after create + 2× sync on A"
