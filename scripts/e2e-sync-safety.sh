#!/usr/bin/env bash
# Safety e2e: under chaos, naive file-level git sync should break repos; composite should not.
#
# Two simulated clients (DOJJO_HOME_A / DOJJO_HOME_B), cold join on B. See README.md and docs/DEVELOPMENT.md.
#
# Health = only git/jj commands that confirm the workspace is usable (log, show, st, fsck, cat-file).
#
# Run on the revision under test (e.g. `jj new nrnwxtry` or `jj new xqwolzls`):
#   bash scripts/e2e-sync-safety.sh
#
# Override: DOJJO_E2E_EXPECT=naive|composite
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
"${ROOT}/scripts/init-sync-server-db.sh"
export DATABASE_URL="sqlite://${ROOT}/sync-server/data.db"
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
if [[ -z "$JJ" ]] || [[ -z "$GIT" ]]; then
  echo "jj and git must be on PATH" >&2
  exit 1
fi

if [[ -f "${ROOT}/client/src/git_sync.rs" ]]; then
  SYNC_MODE="${DOJJO_E2E_EXPECT:-composite}"
else
  SYNC_MODE="${DOJJO_E2E_EXPECT:-naive}"
fi
case "$SYNC_MODE" in
  naive | composite) ;;
  *)
    echo "DOJJO_E2E_EXPECT must be naive or composite (got $SYNC_MODE)" >&2
    exit 1
    ;;
esac
echo "e2e sync mode: $SYNC_MODE"

# shellcheck source=scripts/e2e-lib.sh
source "${ROOT}/scripts/e2e-lib.sh"

TMP="$(mktemp -d)"
e2e_init_peer_homes "$TMP"
SERVER_PID=""
CHAOS_PIDS=()

cleanup() {
  local pid
  for pid in ${CHAOS_PIDS+"${CHAOS_PIDS[@]}"}; do
    kill "$pid" 2>/dev/null || true
  done
  e2e_stop_server
  rm -rf "$TMP"
}
trap cleanup EXIT

resolve_git_dir_from_repo() {
  e2e_resolve_git_dir_from_repo "$1"
}

start_server() {
  e2e_start_server "$TMP" "$1"
  e2e_init_dojjo_clients
}

stop_server() {
  e2e_stop_server
}

# 0 = healthy, 1 = broken. Only git and jj probes.
repo_health() {
  local ws="$1"
  local label="${2:-$ws}"
  local git_dir
  git_dir="$(resolve_git_dir_from_repo "${ws}/.jj/repo")"

  if ! (cd "$ws" && "$JJ" log -n 8 >/dev/null 2>&1); then
    [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[$label] jj log failed" >&2
    return 1
  fi
  if ! (cd "$ws" && "$JJ" st >/dev/null 2>&1); then
    [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[$label] jj st failed" >&2
    return 1
  fi
  if (cd "$ws" && "$JJ" log -r '@-' -n 1 >/dev/null 2>&1); then
    if ! (cd "$ws" && "$JJ" show -r '@-' >/dev/null 2>&1); then
      [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[$label] jj show @- failed" >&2
      return 1
    fi
    if ! (cd "$ws" && "$JJ" diff -r '@-' -r '@' >/dev/null 2>&1); then
      [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[$label] jj diff @- @ failed" >&2
      return 1
    fi
  fi

  if ! "$GIT" -C "$git_dir" fsck --no-progress >/dev/null 2>&1; then
    [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[$label] git fsck failed" >&2
    return 1
  fi

  # Non-colocated: verify @- is materialized in Git when it is a real commit (not root()).
  if (cd "$ws" && "$JJ" log -r '@-' -n 1 >/dev/null 2>&1); then
    local cid tree
    cid="$(e2e_jj_commit_id_at "$ws" '@-')"
    if [[ -n "$cid" ]] && ! e2e_is_jj_root_commit_id "$cid"; then
      if ! "$GIT" -C "$git_dir" cat-file -e "${cid}^{commit}" 2>/dev/null; then
        [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[$label] git cat-file @- commit failed ($cid)" >&2
        return 1
      fi
      tree="$("$GIT" -C "$git_dir" rev-parse "${cid}^{tree}")"
      if ! "$GIT" -C "$git_dir" cat-file -e "$tree" 2>/dev/null; then
        [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[$label] git cat-file @- tree failed ($cid)" >&2
        return 1
      fi
    fi
  fi

  return 0
}

# Both peers healthy, and clone can materialize origin's parent commit (jj only).
two_peer_health() {
  local origin="$1"
  local clone="$2"
  repo_health "$origin" "origin" || return 1
  repo_health "$clone" "clone" || return 1
  local tip
  tip="$(e2e_jj_commit_id_at "$origin" '@-')"
  if [[ -z "$tip" ]]; then
    [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[clone] no origin @- commit_id" >&2
    return 1
  fi
  if ! (cd "$clone" && "$JJ" show -r "$tip" >/dev/null 2>&1); then
    [[ -n "${DOJJO_E2E_DEBUG:-}" ]] && echo "[clone] jj show origin tip $tip failed" >&2
    return 1
  fi
  return 0
}

stabilize_peers() {
  local rounds="${1:-8}"
  local r
  for r in $(seq 1 "$rounds"); do
    (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null)
    (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null)
  done
  (cd "$ORIGIN" && "$JJ" workspace update-stale >/dev/null 2>&1) || true
  (cd "$CLONE" && "$JJ" workspace update-stale >/dev/null 2>&1) || true
  (cd "$ORIGIN" && "$JJ" git export >/dev/null 2>&1) || true
  (cd "$CLONE" && "$JJ" git export >/dev/null 2>&1) || true
  (cd "$ORIGIN" && "$JJ" git import >/dev/null 2>&1) || true
  (cd "$CLONE" && "$JJ" git import >/dev/null 2>&1) || true
}

init_non_colocated_pair() {
  local tag="$1"
  local peer_b_name="peer-b-${tag}"
  ORIGIN="${TMP}/origin-${tag}"
  CLONE="${TMP}/clone-${tag}"
  NEUTRAL="${TMP}/neutral-${tag}"
  rm -rf "$ORIGIN" "$CLONE" "$NEUTRAL"
  mkdir -p "$ORIGIN" "$NEUTRAL"
  (cd "$ORIGIN" && "$JJ" git init --no-colocate)
  echo "seed" > "${ORIGIN}/README.md"
  (cd "$ORIGIN" && "$JJ" file track README.md && "$JJ" commit -m "init")

  OUT="$(cd "${ORIGIN}" && e2e_dojjo_for_home "$DOJJO_HOME_A" create 2>&1)"
  DOJO_ID="$(echo "$OUT" | sed -n 's/.*created dojo \([^ ;]*\).*/\1/p')"
  if [[ -z "$DOJO_ID" ]]; then
    echo "failed to parse dojo id: $OUT" >&2
    exit 1
  fi

  DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_A" "$DOJO_ID")"
  echo "peer A local repo: $DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A"

  rm -rf "$CLONE"
  mkdir -p "$CLONE"
  e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "$peer_b_name" "$NEUTRAL"

  DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_B" "$DOJO_ID")"
  echo "peer B local repo: $DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B"
  e2e_assert_distinct_paths "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B"

  (cd "$CLONE" && "$JJ" workspace update-stale >/dev/null 2>&1) || true
}

chaos_background_object_rsync() {
  local src_git="$1"
  local dst_git="$2"
  local stop_file="$3"
  while [[ ! -f "$stop_file" ]]; do
    if [[ -d "${src_git}/objects" ]]; then
      mkdir -p "${dst_git}/objects"
      rsync -a --delete --ignore-errors "${src_git}/objects/" "${dst_git}/objects/" 2>/dev/null || true
    fi
    sleep 0.03
  done
}

chaos_partial_server_mirror_overlay() {
  local dojo_id="$1"
  local clone_repo="$2"
  # clone_repo must be a physical directory (peer B local repo jj_repo_folder), not a .jj/repo pointer file.
  if [[ -f "$clone_repo" ]]; then
    echo "chaos_partial_server_mirror_overlay: expected directory, got pointer file $clone_repo" >&2
    return 1
  fi
  local mirror_root="${TMP}/dojjo-data/${dojo_id}/mirror"
  [[ -d "$mirror_root" ]] || return 0
  find "$mirror_root" -type f 2>/dev/null | head -40 | while read -r f; do
    rel="${f#"${mirror_root}/"}"
    dest="${clone_repo}/${rel}"
    mkdir -p "$(dirname "$dest")"
    cp -f "$f" "$dest" 2>/dev/null || true
  done
}

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

# Truncate a pack that already exists on victim (no repack — keeps mirror path stable for peers).
inject_torn_packfile_shared() {
  local victim_git="$1"
  local donor_git="$2"
  chmod -R u+w "${victim_git}/objects" 2>/dev/null || true
  local pack base size half victim_pack
  pack="$(find "${donor_git}/objects/pack" -name '*.pack' 2>/dev/null | head -1)" || return 1
  [[ -n "$pack" ]] || return 1
  base="$(basename "$pack")"
  victim_pack="${victim_git}/objects/pack/${base}"
  [[ -f "$victim_pack" ]] || return 1
  size="$(wc -c <"$pack" | tr -d ' ')"
  half=$((size / 2))
  [[ "$half" -gt 0 ]] || half=1
  rm -f "${victim_pack}" "${victim_pack%.pack}.idx"
  head -c "$half" "$pack" >"${victim_pack}"
  rm -f "${victim_pack%.pack}.idx"
  find "${victim_git}/objects" -maxdepth 1 -type d -name '[0-9a-f][0-9a-f]' -exec rm -rf {} + 2>/dev/null || true
  return 0
}

strip_git_alternates() {
  local git_dir="$1"
  rm -f "${git_dir}/objects/info/alternates"
}

inject_refs_without_objects() {
  local victim_git="$1"
  local donor_git="$2"
  rm -rf "${victim_git}/objects"
  mkdir -p "${victim_git}/objects"
  if [[ -d "${donor_git}/refs" ]]; then
    rm -rf "${victim_git}/refs"
    cp -a "${donor_git}/refs" "${victim_git}/refs"
  fi
  if [[ -f "${donor_git}/HEAD" ]]; then
    cp "${donor_git}/HEAD" "${victim_git}/HEAD"
  fi
  if [[ -f "${donor_git}/config" ]]; then
    cp "${donor_git}/config" "${victim_git}/config"
  fi
}

# Returns 0 if healthy, 1 if broken.

scenario_concurrent_objects_during_sync() {
  local tag="$1"
  echo "--- chaos A: concurrent commits + sync (+ naive object copy if naive) ---"
  start_server "-chaos-${tag}"
  init_non_colocated_pair "$tag"
  # Baseline convergence before chaos (two physical repos, bridge-only).
  stabilize_peers 4

  local origin_git clone_git stop
  origin_git="$(resolve_git_dir_from_repo "${ORIGIN}/.jj/repo")"
  clone_git="$(resolve_git_dir_from_repo "${CLONE}/.jj/repo")"
  stop="${TMP}/stop-${tag}"
  rm -f "$stop"

  chaos_background_commits "$ORIGIN" "$stop" &
  CHAOS_PIDS+=($!)
  if [[ "$SYNC_MODE" == "naive" ]]; then
    chaos_background_object_rsync "$origin_git" "$clone_git" "$stop" &
    CHAOS_PIDS+=($!)
  fi

  local round
  for round in $(seq 1 12); do
    (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null 2>&1) || true
    if [[ "$SYNC_MODE" == "naive" ]]; then
      chaos_partial_server_mirror_overlay "$DOJO_ID" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B"
    fi
    (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null 2>&1) || true
    sleep 0.1
  done

  touch "$stop"
  if ((${#CHAOS_PIDS[@]} > 0)); then
    wait "${CHAOS_PIDS[@]}" 2>/dev/null || true
  fi
  CHAOS_PIDS=()

  if [[ "$SYNC_MODE" == "composite" ]]; then
    stabilize_peers 16
  fi

  if two_peer_health "$ORIGIN" "$CLONE"; then
    stop_server
    return 0
  fi
  stop_server
  return 1
}

scenario_torn_packfile() {
  local tag="$1"
  echo "--- chaos B: torn packfile on clone ---"
  start_server "-torn-${tag}"
  init_non_colocated_pair "$tag"

  local origin_git clone_git
  origin_git="$(resolve_git_dir_from_repo "${ORIGIN}/.jj/repo")"
  clone_git="$(resolve_git_dir_from_repo "${CLONE}/.jj/repo")"

  local n
  for n in $(seq 1 5); do
    echo "line $n" >> "${ORIGIN}/README.md"
    (cd "$ORIGIN" && "$JJ" commit -m "c${n}" >/dev/null)
  done
  stabilize_peers 4

  if ! repo_health "$CLONE" "clone-pre"; then
    echo "clone not healthy before torn-pack inject (setup bug)" >&2
    stop_server
    return 1
  fi

  inject_torn_packfile "$clone_git" "$origin_git"
  strip_git_alternates "$clone_git"
  if repo_health "$CLONE" "clone-post"; then
    stop_server
    return 0
  fi
  stop_server
  return 1
}

scenario_refs_without_objects() {
  local tag="$1"
  echo "--- chaos C: refs without objects on clone ---"
  start_server "-refs-${tag}"
  init_non_colocated_pair "$tag"

  local origin_git clone_git
  origin_git="$(resolve_git_dir_from_repo "${ORIGIN}/.jj/repo")"
  clone_git="$(resolve_git_dir_from_repo "${CLONE}/.jj/repo")"

  stabilize_peers 4
  inject_refs_without_objects "$clone_git" "$origin_git"
  strip_git_alternates "$clone_git"
  if repo_health "$CLONE" "clone"; then
    stop_server
    return 0
  fi
  stop_server
  return 1
}

# A has a broken git store, syncs to server, then B syncs. Only B's end health matters.
scenario_bad_origin_sync_then_clone() {
  local tag="$1"
  echo "--- chaos D: broken origin syncs, then clone syncs ---"
  start_server "-propagate-${tag}"
  init_non_colocated_pair "$tag"

  local origin_git clone_git
  origin_git="$(resolve_git_dir_from_repo "${ORIGIN}/.jj/repo")"
  clone_git="$(resolve_git_dir_from_repo "${CLONE}/.jj/repo")"

  local n
  for n in $(seq 1 5); do
    echo "line $n" >> "${ORIGIN}/README.md"
    (cd "$ORIGIN" && "$JJ" commit -m "d${n}" >/dev/null)
  done
  stabilize_peers 4

  if ! repo_health "$CLONE" "clone-pre"; then
    echo "clone not healthy before bad-origin propagate (setup bug)" >&2
    stop_server
    return 1
  fi

  if [[ "$SYNC_MODE" == "naive" ]]; then
    if ! inject_torn_packfile_shared "$origin_git" "$clone_git"; then
      echo "no shared pack to corrupt on origin (setup bug)" >&2
      stop_server
      return 1
    fi
  else
    inject_torn_packfile "$origin_git" "$clone_git"
  fi
  strip_git_alternates "$origin_git"
  if repo_health "$ORIGIN" "origin-post-inject"; then
    echo "origin still healthy after torn-pack inject (setup bug)" >&2
    stop_server
    return 1
  fi

  # A pushes corruption to the server; only B's sync rounds matter for the outcome.
  local round
  for round in $(seq 1 3); do
    (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null 2>&1) || true
  done
  for round in $(seq 1 8); do
    (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null 2>&1) || true
  done
  if [[ "$SYNC_MODE" == "composite" ]]; then
    (cd "$CLONE" && "$JJ" workspace update-stale >/dev/null 2>&1) || true
    (cd "$CLONE" && "$JJ" git export >/dev/null 2>&1) || true
    (cd "$CLONE" && "$JJ" git import >/dev/null 2>&1) || true
    for round in $(seq 1 4); do
      (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null 2>&1) || true
    done
  fi

  if repo_health "$CLONE" "clone-post"; then
    stop_server
    return 0
  fi
  stop_server
  return 1
}

scenario_composite_recovers_after_torn_pack() {
  local tag="$1"
  echo "--- chaos B2: composite recovery after torn pack ---"
  start_server "-recover-${tag}"
  init_non_colocated_pair "$tag"

  local origin_git clone_git
  origin_git="$(resolve_git_dir_from_repo "${ORIGIN}/.jj/repo")"
  clone_git="$(resolve_git_dir_from_repo "${CLONE}/.jj/repo")"

  local n
  for n in $(seq 1 4); do
    echo "x" >> "${ORIGIN}/README.md"
    (cd "$ORIGIN" && "$JJ" commit -m "r${n}" >/dev/null)
  done
  stabilize_peers 4

  inject_torn_packfile "$clone_git" "$origin_git"
  strip_git_alternates "$clone_git"
  if repo_health "$CLONE" "clone-broken"; then
    echo "expected torn pack to break clone before recovery" >&2
    stop_server
    return 1
  fi

  stabilize_peers 6
  if repo_health "$CLONE" "clone-healed"; then
    stop_server
    return 0
  fi
  stop_server
  return 1
}

# broken=0 healthy, broken=1 repo broken

expect_chaos_result() {
  local name="$1"
  local broken="$2"
  if [[ "$SYNC_MODE" == "naive" ]]; then
    if [[ "$broken" -eq 0 ]]; then
      echo "FAIL [$name]: naive stayed healthy (unsafe)" >&2
      exit 1
    fi
    echo "ok [$name]: naive unhealthy (expected)"
  else
    if [[ "$broken" -eq 0 ]]; then
      echo "ok [$name]: composite healthy"
    else
      echo "FAIL [$name]: composite unhealthy" >&2
      exit 1
    fi
  fi
}

expect_injection_breaks_then_maybe_recovers() {
  local break_name="$1"
  local broke="$2"
  local recover_name="${3:-}"
  local recovered="${4:-}"

  if [[ "$broke" -eq 0 ]]; then
    echo "FAIL [$break_name]: injection did not break clone" >&2
    exit 1
  fi
  echo "ok [$break_name]: injection broke clone"

  if [[ "$SYNC_MODE" == "composite" && -n "$recover_name" ]]; then
    if [[ "$recovered" -eq 1 ]]; then
      echo "FAIL [$recover_name]: composite did not heal after sync" >&2
      exit 1
    fi
    echo "ok [$recover_name]: composite healed (jj/git healthy after sync)"
  fi
}

broken=1
if scenario_concurrent_objects_during_sync "A"; then broken=0; fi
expect_chaos_result "concurrent sync under chaos" "$broken"

broken=1
if scenario_torn_packfile "B"; then broken=0; fi
if [[ "$SYNC_MODE" == "composite" ]]; then
  recovered=1
  if scenario_composite_recovers_after_torn_pack "B2"; then recovered=0; fi
  expect_injection_breaks_then_maybe_recovers "torn packfile" "$broken" "re-sync after torn pack" "$recovered"
else
  expect_injection_breaks_then_maybe_recovers "torn packfile" "$broken"
fi

broken=1
if scenario_refs_without_objects "C"; then broken=0; fi
expect_injection_breaks_then_maybe_recovers "refs without objects" "$broken"

broken=1
if scenario_bad_origin_sync_then_clone "D"; then broken=0; fi
expect_chaos_result "broken origin sync then clone sync" "$broken"

echo "e2e sync safety OK (mode=$SYNC_MODE)"
