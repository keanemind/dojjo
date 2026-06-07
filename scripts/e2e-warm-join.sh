#!/usr/bin/env bash
# Regression: warm join on same DOJJO_HOME must not reintroduce stale op_heads removed by JJ.
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

e2e_start_server "$TMP" "-warm-join"
e2e_init_dojjo_clients

ORIGIN="${TMP}/origin"
SECOND="${TMP}/second"
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

e2e_stop_background_sync_for_home "$DOJJO_HOME_A"
(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync)

SYNC_REPO="$(e2e_physical_sync_repo "$DOJJO_HOME_A" "$DOJO_ID")"
OP_BEFORE="$(e2e_jj_current_operation_id "$ORIGIN")"
if [[ -z "$OP_BEFORE" ]]; then
  echo "failed to read operation id before local commit" >&2
  exit 1
fi

echo "local edit (no sync)…" >> "${ORIGIN}/README.md"
(cd "$ORIGIN" && "$JJ" commit -m "local-only")
OP_AFTER="$(e2e_jj_current_operation_id "$ORIGIN")"
if [[ -z "$OP_AFTER" ]]; then
  echo "failed to read operation id after local commit" >&2
  exit 1
fi
if [[ "$OP_BEFORE" == "$OP_AFTER" ]]; then
  echo "expected new operation after local commit (still $OP_AFTER)" >&2
  exit 1
fi

STALE_HEAD="${SYNC_REPO}/op_heads/heads/${OP_BEFORE}"
if [[ -f "$STALE_HEAD" ]]; then
  echo "expected stale op head $OP_BEFORE removed after local commit" >&2
  exit 1
fi
if [[ ! -f "${SYNC_REPO}/op_store/operations/${OP_AFTER}" ]]; then
  echo "expected local op $OP_AFTER in op_store" >&2
  exit 1
fi

rm -rf "$SECOND"
mkdir -p "$SECOND"
JOIN_OUT="$(cd "$NEUTRAL" && e2e_dojjo_for_home "$DOJJO_HOME_A" join \
  --dojo-id "$DOJO_ID" \
  --into "$SECOND" \
  --name "second-ws" 2>&1)"
echo "$JOIN_OUT"
if ! echo "$JOIN_OUT" | grep -q "warm join"; then
  echo "expected warm join message in join output" >&2
  exit 1
fi

if [[ -f "$STALE_HEAD" ]]; then
  echo "warm join must not recreate stale op head $OP_BEFORE" >&2
  exit 1
fi
if [[ ! -f "${SYNC_REPO}/op_store/operations/${OP_AFTER}" ]]; then
  echo "warm join must preserve local op $OP_AFTER in op_store" >&2
  exit 1
fi

LOCAL_CHANGE="$(cd "$ORIGIN" && "$JJ" log -r '@-' -n 1 --no-graph -T 'change_id' 2>/dev/null | tr -d '[:space:]')"
if [[ -z "$LOCAL_CHANGE" ]]; then
  echo "failed to read local-only change id from origin" >&2
  exit 1
fi
if ! (cd "$SECOND" && "$JJ" log -r "$LOCAL_CHANGE" -n 1 --no-graph >/dev/null 2>&1); then
  echo "second workspace must see local-only change $LOCAL_CHANGE" >&2
  exit 1
fi

echo "e2e-warm-join: warm join ok; stale op head not reintroduced"
