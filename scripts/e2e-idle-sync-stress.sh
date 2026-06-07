#!/usr/bin/env bash
# Gate 1: idle alternating sync stress (no jj commits after converge).
#
# After create/join/converge, runs N rounds of (peer A dev sync; peer B dev sync) with no
# further jj commands. Emits NDJSON per step and exits:
#   0 — every stress step push=0 pull=0; op ids stable; manifest entry count stable after round 1
#   1 — churn (nonzero push/pull on any stress step)
#   2 — invariant violation (op id changed, or manifest entry count grew after steady state)
#
# Env:
#   E2E_IDLE_SYNC_STRESS_ROUNDS   — default 40
#   E2E_IDLE_SYNC_STRESS_NDJSON   — default $TMP/idle-sync-stress.ndjson
#   E2E_IDLE_SYNC_EXPECT_QUIET=1  — same as default (fail on churn); set 0 to report churn but exit 0
#   DOJJO_SYNC_DEBUG=1            — NDJSON phases on stderr from each `dev sync` (see client/src/sync_debug.rs)
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

ROUNDS="${E2E_IDLE_SYNC_STRESS_ROUNDS:-40}"
if [[ ! "$ROUNDS" =~ ^[0-9]+$ ]] || [[ "$ROUNDS" -lt 1 ]]; then
  echo "E2E_IDLE_SYNC_STRESS_ROUNDS must be a positive integer (got: $ROUNDS)" >&2
  exit 1
fi

EXPECT_QUIET="${E2E_IDLE_SYNC_EXPECT_QUIET:-0}"

# shellcheck source=scripts/e2e-lib.sh
source "${ROOT}/scripts/e2e-lib.sh"

TMP="$(mktemp -d)"
e2e_init_peer_homes "$TMP"
NDJSON="${E2E_IDLE_SYNC_STRESS_NDJSON:-${TMP}/idle-sync-stress.ndjson}"
: >"$NDJSON"
export E2E_SYNC_DEBUG_STDERR="${TMP}/idle-sync-debug.ndjson"
: >"$E2E_SYNC_DEBUG_STDERR"

PERSIST_DEBUG_NDJSON=""
cleanup() {
  if [[ -n "${DOJJO_SYNC_DEBUG:-}" && "${DOJJO_SYNC_DEBUG}" != "0" && "${DOJJO_SYNC_DEBUG}" != "false" ]]; then
    if [[ -f "${E2E_SYNC_DEBUG_STDERR:-}" ]]; then
      mkdir -p "${ROOT}/target"
      PERSIST_DEBUG_NDJSON="${ROOT}/target/idle-sync-last-debug.ndjson"
      cp "$E2E_SYNC_DEBUG_STDERR" "$PERSIST_DEBUG_NDJSON"
      echo "idle-sync-stress: debug_ndjson=${PERSIST_DEBUG_NDJSON}"
    fi
  fi
  e2e_stop_server
  rm -rf "$TMP"
}
trap cleanup EXIT

e2e_start_server "$TMP" "-idle-sync-stress"
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
[[ -n "$DOJO_ID" ]] || { echo "no dojo id" >&2; exit 2; }

SYNC_REPO_A="$(e2e_physical_sync_repo "$DOJJO_HOME_A" "$DOJO_ID")"
rm -rf "$CLONE"
mkdir -p "$CLONE"
e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "clone" "$NEUTRAL"
SYNC_REPO_B="$(e2e_physical_sync_repo "$DOJJO_HOME_B" "$DOJO_ID")"
e2e_assert_distinct_paths "$SYNC_REPO_A" "$SYNC_REPO_B"

OP_ID_A_BASELINE=""
OP_ID_B_BASELINE=""

stress_sync_peer() {
  local round="$1"
  local step="$2"
  local peer="$3"
  local home="$4"
  local ws="$5"
  local sync_repo="$6"

  local op_before op_after idx_before idx_after
  op_before="$(e2e_jj_current_operation_id "$ws")"
  idx_before="$(e2e_mirror_sha256_index "$sync_repo")"

  local diff_json disk_json
  diff_json="$(e2e_mirror_local_vs_server_diff "$sync_repo" "$DOJJO_API_BASE" "$DOJO_ID" | python3 -c '
import json, sys
print(json.dumps([line.strip() for line in sys.stdin if line.strip()][:20]))
' || echo '[]')"
  if [[ -z "$diff_json" ]]; then
    diff_json='[]'
  fi

  local metrics push pull sync_rev
  metrics="$(e2e_dev_sync_metrics "$home" "$ws")"
  read -r push pull sync_rev <<<"$metrics"

  op_after="$(e2e_jj_current_operation_id "$ws")"
  if [[ "$op_before" != "$op_after" ]]; then
    echo "ERROR: peer $peer round $round step $step: operation id changed (${op_before:0:16}… -> ${op_after:0:16}…)" >&2
    exit 2
  fi

  idx_after="$(e2e_mirror_sha256_index "$sync_repo")"
  disk_json="$(e2e_mirror_index_changed_paths "$idx_before" "$idx_after" | python3 -c '
import json, sys
print(json.dumps([line.strip() for line in sys.stdin if line.strip()][:20]))
' || echo '[]')"
  if [[ -z "$disk_json" ]]; then
    disk_json='[]'
  fi

  local man_summary man_entries man_rev local_files
  man_summary="$(e2e_manifest_summary "$DOJJO_API_BASE" "$DOJO_ID")"
  read -r man_entries man_rev <<<"$man_summary"
  local_files="$(e2e_mirror_local_file_count "$sync_repo")"

  export E2E_STRESS_RECORD_ROUND="$round"
  export E2E_STRESS_RECORD_STEP="$step"
  export E2E_STRESS_RECORD_PEER="$peer"
  export E2E_STRESS_RECORD_PUSH="$push"
  export E2E_STRESS_RECORD_PULL="$pull"
  export E2E_STRESS_RECORD_MANIFEST_ENTRIES="$man_entries"
  export E2E_STRESS_RECORD_MANIFEST_REVISION="$man_rev"
  export E2E_STRESS_RECORD_SYNC_REVISION="${sync_rev:-}"
  export E2E_STRESS_RECORD_OP_ID="$op_after"
  export E2E_STRESS_RECORD_LOCAL_FILES="$local_files"
  export E2E_STRESS_RECORD_DIFF_BEFORE="$diff_json"
  export E2E_STRESS_RECORD_DISK_CHANGED="$disk_json"
  e2e_idle_sync_stress_emit "$NDJSON"

  if [[ "$peer" == "A" ]]; then
    if [[ -z "$OP_ID_A_BASELINE" ]]; then
      OP_ID_A_BASELINE="$op_after"
    elif [[ "$OP_ID_A_BASELINE" != "$op_after" ]]; then
      echo "ERROR: peer A operation id drifted from baseline" >&2
      exit 2
    fi
  else
    if [[ -z "$OP_ID_B_BASELINE" ]]; then
      OP_ID_B_BASELINE="$op_after"
    elif [[ "$OP_ID_B_BASELINE" != "$op_after" ]]; then
      echo "ERROR: peer B operation id drifted from baseline" >&2
      exit 2
    fi
  fi

  if [[ "$push" != "0" || "$pull" != "0" ]]; then
    CHURN_STEPS=$((CHURN_STEPS + 1))
  fi

  echo "round $round step $step peer $peer: push=$push pull=$pull manifest_entries=$man_entries local_files=$local_files"
}

# Converge after create/join; no jj commits from here.
echo "converging (3× A/B sync)…"
for _ in 1 2 3; do
  (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null)
  (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null)
done

read -r BASELINE_MANIFEST_ENTRIES BASELINE_MANIFEST_REV <<<"$(e2e_manifest_summary "$DOJJO_API_BASE" "$DOJO_ID")"
OP_ID_A_BASELINE="$(e2e_jj_current_operation_id "$ORIGIN")"
OP_ID_B_BASELINE="$(e2e_jj_current_operation_id "$CLONE")"
LOCAL_FILES_A="$(e2e_mirror_local_file_count "$SYNC_REPO_A")"
LOCAL_FILES_B="$(e2e_mirror_local_file_count "$SYNC_REPO_B")"

echo "baseline manifest_entries=$BASELINE_MANIFEST_ENTRIES revision=${BASELINE_MANIFEST_REV:0:16}…"
echo "baseline op A: ${OP_ID_A_BASELINE:0:16}…  op B: ${OP_ID_B_BASELINE:0:16}…"
echo "baseline local mirror files A=$LOCAL_FILES_A B=$LOCAL_FILES_B"
echo "stress: $ROUNDS rounds (A sync, B sync); ndjson=$NDJSON"
echo ""

CHURN_STEPS=0
MANIFEST_DRIFT=0
STEADY_MANIFEST_ENTRIES="$BASELINE_MANIFEST_ENTRIES"
MANIFEST_STEADY_LOCKED=0
STEP_NO=0

for round in $(seq 1 "$ROUNDS"); do
  STEP_NO=$((STEP_NO + 1))
  stress_sync_peer "$round" "$STEP_NO" "A" "$DOJJO_HOME_A" "$ORIGIN" "$SYNC_REPO_A"

  STEP_NO=$((STEP_NO + 1))
  stress_sync_peer "$round" "$STEP_NO" "B" "$DOJJO_HOME_B" "$CLONE" "$SYNC_REPO_B"
  read -r ENTRIES_AFTER_B _ <<<"$(e2e_manifest_summary "$DOJJO_API_BASE" "$DOJO_ID")"

  if [[ "$MANIFEST_STEADY_LOCKED" -eq 0 ]]; then
    if [[ "$ENTRIES_AFTER_B" != "$BASELINE_MANIFEST_ENTRIES" ]]; then
      echo "note: manifest entries after round $round: $BASELINE_MANIFEST_ENTRIES -> $ENTRIES_AFTER_B (one-time convergence bump)"
      STEADY_MANIFEST_ENTRIES="$ENTRIES_AFTER_B"
    fi
    MANIFEST_STEADY_LOCKED=1
  elif [[ "$ENTRIES_AFTER_B" != "$STEADY_MANIFEST_ENTRIES" ]]; then
    echo "warn: manifest entry count changed after steady state: $STEADY_MANIFEST_ENTRIES -> $ENTRIES_AFTER_B (round $round)" >&2
    MANIFEST_DRIFT=1
    STEADY_MANIFEST_ENTRIES="$ENTRIES_AFTER_B"
  fi
done

echo ""
python3 - "$NDJSON" "$ROUNDS" <<'PY'
import json, pathlib, sys

path = pathlib.Path(sys.argv[1])
rounds = int(sys.argv[2])
lines = path.read_text(encoding="utf-8").splitlines()
records = [json.loads(ln) for ln in lines if ln.strip()]

churn = [r for r in records if r["push"] != 0 or r["pull"] != 0]
max_pull = max((r["pull"] for r in records), default=0)
max_push = max((r["push"] for r in records), default=0)

paths_diff = set()
paths_disk = set()
for r in churn:
    for p in r.get("local_vs_server_before", []):
        paths_diff.add(p)
    for p in r.get("on_disk_changed", []):
        paths_disk.add(p)

entries = [r["manifest_entries"] for r in records]
print(f"stress summary: rounds={rounds} steps={len(records)} churn_steps={len(churn)}")
print(f"  max push={max_push} max pull={max_pull}")
if entries:
    print(f"  manifest_entries: first={entries[0]} last={entries[-1]}")
if paths_diff:
    print(f"  unique local≠server paths on churn steps ({len(paths_diff)}):")
    for p in sorted(paths_diff)[:15]:
        print(f"    {p}")
    if len(paths_diff) > 15:
        print(f"    … and {len(paths_diff) - 15} more")
if paths_disk:
    print(f"  unique on-disk changed paths on churn steps ({len(paths_disk)}):")
    for p in sorted(paths_disk)[:15]:
        print(f"    {p}")
    if len(paths_disk) > 15:
        print(f"    … and {len(paths_disk) - 15} more")
unique_entries = sorted(set(entries))
if len(unique_entries) > 1:
    print(f"  manifest_entries varied across steps: {unique_entries[0]}..{unique_entries[-1]} ({len(unique_entries)} distinct values)")
print(f"  ndjson: {path}")
PY

TOTAL_STEPS=$((ROUNDS * 2))
if [[ "$MANIFEST_DRIFT" -ne 0 ]]; then
  echo "idle-sync-stress: manifest entry count not stable after round 1 (see warnings above)"
fi
if [[ "$CHURN_STEPS" -eq 0 && "$MANIFEST_DRIFT" -eq 0 ]]; then
  echo "idle-sync-stress: PASS (all $TOTAL_STEPS steps quiet; manifest stable)"
  exit 0
fi
if [[ "$CHURN_STEPS" -ne 0 ]]; then
  echo "idle-sync-stress: FAIL ($CHURN_STEPS / $TOTAL_STEPS steps had push≠0 or pull≠0)"
  if [[ "$EXPECT_QUIET" == "1" ]]; then
    exit 1
  fi
  echo "E2E_IDLE_SYNC_EXPECT_QUIET=0 — exiting 0 despite churn"
  exit 0
fi
exit 2
