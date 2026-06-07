#!/usr/bin/env bash
# Forensic repro for chaos-A fetch failures. Does not modify jj state.
# Usage: bash scripts/debug-chaos-a-fetch.sh [attempts]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
ATTEMPTS="${1:-5}"

"${ROOT}/scripts/init-sync-server-db.sh"
export DATABASE_URL="sqlite://${ROOT}/sync-server/data.db"
cargo build -q --bin dojjo --bin sync-server

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
export SYNC_SERVER="${TARGET_DIR}/debug/sync-server"
export DOJJO="${TARGET_DIR}/debug/dojjo"
export JJ="$(command -v jj)"
export GIT="${GIT:-$(command -v git)}"

# shellcheck source=scripts/e2e-lib.sh
source "${ROOT}/scripts/e2e-lib.sh"

forensic_bare() {
  local label="$1"
  local bare="$2"
  local bad_oid="${3:-}"
  echo "=== bare forensics [$label] $bare ==="
  if [[ ! -d "$bare" ]]; then
    echo "  (missing)"
    return
  fi
  echo "  fsck:"
  "$GIT" -C "$bare" fsck --strict 2>&1 | head -20 | sed 's/^/    /' || true
  echo "  heads:"
  "$GIT" -C "$bare" for-each-ref --format='%(refname) %(objectname)' refs/heads refs/jj 2>/dev/null | head -15 | sed 's/^/    /' || true
  if [[ -n "$bad_oid" ]]; then
    echo "  bad_oid $bad_oid on bare:"
    if "$GIT" -C "$bare" cat-file -e "$bad_oid" 2>/dev/null; then
      echo "    object EXISTS on bare"
    else
      echo "    object MISSING on bare"
    fi
    echo "  refs pointing at bad_oid:"
    "$GIT" -C "$bare" for-each-ref --format='%(refname) %(objectname)' | while read -r ref oid; do
      if [[ "$oid" == "$bad_oid" ]]; then
        echo "    $ref"
      fi
    done
  fi
}

forensic_client_git() {
  local label="$1"
  local ws="$2"
  local bad_oid="${3:-}"
  local git_dir
  git_dir="$(e2e_resolve_git_dir_from_repo "${ws}/.jj/repo")"
  echo "=== client git [$label] $git_dir ==="
  echo "  fsck:"
  "$GIT" -C "$git_dir" fsck --strict 2>&1 | head -15 | sed 's/^/    /' || true
  if [[ -n "$bad_oid" ]]; then
    if "$GIT" -C "$git_dir" cat-file -e "$bad_oid" 2>/dev/null; then
      echo "  bad_oid EXISTS locally"
    else
      echo "  bad_oid MISSING locally"
    fi
  fi
}

forensic_mirror_git_refs() {
  local label="$1"
  local mirror="$2"
  local bad_oid="${3:-}"
  echo "=== mirror git refs [$label] $mirror ==="
  if [[ ! -d "$mirror/store/git/refs" ]]; then
    echo "  (no mirror/store/git/refs)"
    return
  fi
  find "$mirror/store/git/refs" -type f 2>/dev/null | while read -r f; do
    local oid
    oid="$(tr -d '[:space:]' <"$f" 2>/dev/null || true)"
    echo "    ${f#"$mirror/"} -> $oid"
    if [[ -n "$bad_oid" && "$oid" == "$bad_oid" ]]; then
      echo "      ^^^ matches bad_oid"
    fi
  done
}

run_chaos_once() {
  local attempt="$1"
  local TMP
  TMP="$(mktemp -d)"
  export TMP
  e2e_init_peer_homes "$TMP"

  cleanup() {
    e2e_stop_server 2>/dev/null || true
    rm -rf "$TMP"
  }
  trap cleanup EXIT

  e2e_start_server "$TMP" "-debug-chaos-${attempt}"
  e2e_init_dojjo_clients

  local tag="A"
  local ORIGIN="${TMP}/origin-${tag}"
  local CLONE="${TMP}/clone-${tag}"
  local NEUTRAL="${TMP}/neutral-${tag}"
  rm -rf "$ORIGIN" "$CLONE" "$NEUTRAL"
  mkdir -p "$ORIGIN" "$NEUTRAL"
  (cd "$ORIGIN" && "$JJ" git init --no-colocate)
  echo "seed" > "${ORIGIN}/README.md"
  (cd "$ORIGIN" && "$JJ" file track README.md && "$JJ" commit -m "init")

  local OUT DOJO_ID
  OUT="$(cd "${ORIGIN}" && e2e_dojjo_for_home "$DOJJO_HOME_A" create 2>&1)"
  DOJO_ID="$(echo "$OUT" | sed -n 's/.*created dojo \([^ ;]*\).*/\1/p')"
  [[ -n "$DOJO_ID" ]] || { echo "no dojo id"; return 2; }

  rm -rf "$CLONE"
  mkdir -p "$CLONE"
  e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "peer-b-${tag}" "$NEUTRAL"

  for _ in 1 2 3 4; do
    (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null)
    (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null)
  done

  local stop="${TMP}/stop"
  rm -f "$stop"
  chaos_background_commits() {
    local ws="$1"
    local stop_file="$2"
    local i=0
    while [[ ! -f "$stop_file" ]]; do
      i=$((i + 1))
      echo "chaos-${i}" >> "${ws}/README.md"
      (cd "$ws" && "$JJ" commit -m "chaos ${i}" >/dev/null 2>&1) || true
      sleep 0.05
    done
  }
  chaos_background_commits "$ORIGIN" "$stop" &
  local chaos_pid=$!

  local round bad_oid="" fail_peer="" fail_out=""
  for round in $(seq 1 12); do
    local out_a out_b
    out_a="$(cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync 2>&1)" || true
    out_b="$(cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync 2>&1)" || true
    if echo "$out_a$out_b" | grep -q 'bad object'; then
      bad_oid="$(echo "$out_a$out_b" | sed -n 's/.*bad object \([0-9a-f]\{40\}\).*/\1/p' | head -1)"
      if echo "$out_a" | grep -q 'bad object'; then fail_peer="origin"; fail_out="$out_a"; else fail_peer="clone"; fail_out="$out_b"; fi
      echo "ATTEMPT $attempt round $round: fetch failure on $fail_peer bad_oid=${bad_oid:-unknown}"
      echo "$fail_out" | tail -8
      touch "$stop"
      wait "$chaos_pid" 2>/dev/null || true

      local bare="${DOJJO_DATA_DIR}/${DOJO_ID}/bare.git"
      local mirror="${DOJJO_DATA_DIR}/${DOJO_ID}/mirror"
      forensic_bare "after-failure" "$bare" "$bad_oid"
      forensic_mirror_git_refs "after-failure" "$mirror" "$bad_oid"
      forensic_client_git "origin" "$ORIGIN" "$bad_oid"
      forensic_client_git "clone" "$CLONE" "$bad_oid"

      echo "=== ls-remote bare (what fetch negotiates against) ==="
      "$GIT" ls-remote "${DOJJO_PUBLIC_URL}/git/${DOJO_ID}.git" 2>&1 | head -15 | sed 's/^/    /' || true

      return 1
    fi
    sleep 0.1
  done

  touch "$stop"
  wait "$chaos_pid" 2>/dev/null || true
  echo "ATTEMPT $attempt: chaos completed without bad object in 12 rounds"
  return 0
}

pass=0
fail=0
for a in $(seq 1 "$ATTEMPTS"); do
  if run_chaos_once "$a"; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    # one detailed failure is enough for first pass
    if [[ "$fail" -ge 1 && "${STOP_AFTER_FIRST:-1}" == "1" ]]; then
      break
    fi
  fi
done

echo ""
echo "SUMMARY: pass=$pass fail=$fail attempts=$ATTEMPTS"
exit "$([[ "$fail" -eq 0 ]] && echo 0 || echo 1)"
