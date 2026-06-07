# Shared helpers for Dojjo bash e2e scripts.
# Usage: source "$(dirname "$0")/e2e-lib.sh"   (or via E2E_LIB path set by caller)
set -euo pipefail

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  echo "source scripts/e2e-lib.sh; do not execute it directly" >&2
  exit 1
fi

# Requires caller to set: JJ, GIT, DOJJO (and optionally TMP for server helpers).

e2e_init_peer_homes() {
  local tmp_root="$1"
  if [[ -z "$tmp_root" ]]; then
    echo "e2e_init_peer_homes: tmp_root required" >&2
    return 1
  fi
  export DOJJO_HOME_A="${tmp_root}/dojjo-home-a"
  export DOJJO_HOME_B="${tmp_root}/dojjo-home-b"
  mkdir -p "$DOJJO_HOME_A" "$DOJJO_HOME_B"
}

e2e_canonicalize_path() {
  python3 - "$1" <<'PY'
import pathlib, sys
print(pathlib.Path(sys.argv[1]).resolve())
PY
}

# Physical `.jj/repo` directory at the hidden sync host (not a pointer file).
e2e_physical_sync_repo() {
  local home="$1"
  local dojo_id="$2"
  if [[ -z "$home" || -z "$dojo_id" ]]; then
    echo "e2e_physical_sync_repo: home and dojo_id required" >&2
    return 1
  fi
  local jj_repo="${home}/dojos/${dojo_id}/_default/.jj/repo"
  if [[ ! -e "$jj_repo" ]]; then
    echo "missing sync repo at $jj_repo" >&2
    return 1
  fi
  e2e_canonicalize_path "$jj_repo"
}

e2e_assert_distinct_paths() {
  local a="$1"
  local b="$2"
  if [[ -z "$a" || -z "$b" ]]; then
    echo "e2e_assert_distinct_paths: two paths required" >&2
    return 1
  fi
  local ca cb
  ca="$(e2e_canonicalize_path "$a")"
  cb="$(e2e_canonicalize_path "$b")"
  if [[ "$ca" == "$cb" ]]; then
    echo "paths must be distinct but are the same: $ca" >&2
    return 1
  fi
  local ia ib
  if stat -f '%i' "$ca" >/dev/null 2>&1; then
    ia="$(stat -f '%i' "$ca")"
    ib="$(stat -f '%i' "$cb")"
  else
    ia="$(stat -c '%i' "$ca")"
    ib="$(stat -c '%i' "$cb")"
  fi
  if [[ "$ia" == "$ib" ]]; then
    echo "same inode ($ia) for peer sync repos:" >&2
    echo "  A: $ca" >&2
    echo "  B: $cb" >&2
    return 1
  fi
}

e2e_dojjo_for_home() {
  local home="$1"
  shift
  if [[ -z "$home" ]]; then
    echo "e2e_dojjo_for_home: home required" >&2
    return 1
  fi
  env DOJJO_HOME="$home" ${DOJJO_SYNC_DEBUG:+"DOJJO_SYNC_DEBUG=$DOJJO_SYNC_DEBUG"} "$DOJJO" "$@"
}

# Stop background sync workers so manual `dev sync` steps are not racing mirror pull/prune.
e2e_stop_background_sync_for_home() {
  local home="$1"
  if [[ -z "$home" ]]; then
    echo "e2e_stop_background_sync_for_home: home required" >&2
    return 1
  fi
  if [[ ! -d "${home}/dojos" ]]; then
    return 0
  fi
  local dojo_dir pid pidfile
  for dojo_dir in "${home}/dojos"/*; do
    [[ -d "$dojo_dir" ]] || continue
    pidfile="${dojo_dir}/background_sync.pid"
    if [[ ! -f "$pidfile" ]]; then
      continue
    fi
    pid="$(tr -d '[:space:]' <"$pidfile")"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
    rm -f "$pidfile"
  done
}

e2e_stop_all_background_sync() {
  if [[ -n "${DOJJO_HOME_A:-}" ]]; then
    e2e_stop_background_sync_for_home "$DOJJO_HOME_A"
  fi
  if [[ -n "${DOJJO_HOME_B:-}" ]]; then
    e2e_stop_background_sync_for_home "$DOJJO_HOME_B"
  fi
}

e2e_cold_join() {
  local home="$1"
  local dojo_id="$2"
  local into="$3"
  local workspace_name="$4"
  local neutral_dir="$5"
  if [[ -z "$home" || -z "$dojo_id" || -z "$into" || -z "$workspace_name" || -z "$neutral_dir" ]]; then
    echo "e2e_cold_join: missing argument" >&2
    return 1
  fi
  (cd "$neutral_dir" && env DOJJO_HOME="$home" "$DOJJO" join \
    --dojo-id "$dojo_id" \
    --into "$into" \
    --name "$workspace_name")
}

# Write ~/.dojjo/config.json (or $DOJJO_HOME/config.json) with api_base. Requires DOJJO_API_BASE.
e2e_init_dojjo_clients() {
  if [[ -z "${DOJJO_API_BASE:-}" ]]; then
    echo "e2e_init_dojjo_clients: DOJJO_API_BASE must be set (run e2e_start_server first)" >&2
    return 1
  fi
  e2e_dojjo_for_home "$DOJJO_HOME_A" init --api-base "$DOJJO_API_BASE"
  e2e_dojjo_for_home "$DOJJO_HOME_B" init --api-base "$DOJJO_API_BASE"
}

e2e_resolve_git_dir_from_repo() {
  local repo_arg="$1"
  python3 - "$repo_arg" <<'PY'
import pathlib, sys
repo = pathlib.Path(sys.argv[1])
if repo.is_file():
    jj = repo.parent
    repo = (jj / repo.read_text().strip()).resolve()
store = repo / "store"
target = (store / "git_target").read_text().strip()
if target == "git":
    git_dir = store / "git"
else:
    git_dir = (store / target).resolve()
print(git_dir.resolve())
PY
}

e2e_jj_commit_id_at() {
  local ws="$1"
  local revset="${2:-@-}"
  (cd "$ws" && "$JJ" log -r "$revset" -n 1 --no-graph -T 'commit_id' 2>/dev/null | head -1 | tr -d '[:space:]')
}

# Current operation id (hex) from `jj op log` — the op the workspace is on.
e2e_jj_current_operation_id() {
  local ws="$1"
  (cd "$ws" && "$JJ" op log -n 1 --no-graph -T 'id' 2>/dev/null | tr -d '[:space:]')
}

# Run `dojjo dev sync` in workspace; print one line: push=N pull=M (0 if absent).
e2e_sync_count_push_pull() {
  e2e_dev_sync_metrics "$@" | awk '{ print $1, $2 }'
}

# Run `dojjo dev sync`; print: push pull manifest_revision (revision empty if absent).
e2e_dev_sync_metrics() {
  local home="$1"
  local ws="$2"
  local out
  local stderr_target="/dev/null"
  if [[ -n "${DOJJO_SYNC_DEBUG:-}" && "${DOJJO_SYNC_DEBUG}" != "0" && "${DOJJO_SYNC_DEBUG}" != "false" ]]; then
    stderr_target="${E2E_SYNC_DEBUG_STDERR:-/dev/stderr}"
  fi
  out="$(cd "$ws" && e2e_dojjo_for_home "$home" dev sync 2>>"$stderr_target")" || {
    echo "$out" >&2
    return 1
  }
  python3 - "$out" <<'PY'
import re, sys

out = sys.argv[1]
push = pull = 0
m = re.search(r"pushing\s+(\d+)", out)
if m:
    push = int(m.group(1))
m = re.search(r"pulling\s+(\d+)", out)
if m:
    pull = int(m.group(1))
rev = ""
m = re.search(r"manifest revision (\S+)", out)
if m:
    rev = m.group(1)
print(push, pull, rev)
PY
}

# Server mirror manifest: print "entry_count revision" (space-separated).
e2e_manifest_summary() {
  local api_base="$1"
  local dojo_id="$2"
  if [[ -z "$api_base" || -z "$dojo_id" ]]; then
    echo "e2e_manifest_summary: api_base and dojo_id required" >&2
    return 1
  fi
  curl -sf "${api_base}/dojo/${dojo_id}/mirror/manifest" | python3 -c '
import json, sys
data = json.load(sys.stdin)
entries = data.get("entries", [])
print(len(entries), data.get("revision", ""))
'
}

# Count mirror-eligible files under physical sync repo (same walk as e2e_mirror_sha256_index).
e2e_mirror_local_file_count() {
  e2e_mirror_sha256_index "$1" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'
}

# Append one NDJSON record for idle-sync stress (fields via env: E2E_STRESS_RECORD_*).
e2e_idle_sync_stress_emit() {
  local ndjson_path="$1"
  python3 - "$ndjson_path" <<'PY'
import json, os, pathlib, sys

path = pathlib.Path(sys.argv[1])
record = {
    "round": int(os.environ["E2E_STRESS_RECORD_ROUND"]),
    "step": int(os.environ["E2E_STRESS_RECORD_STEP"]),
    "peer": os.environ["E2E_STRESS_RECORD_PEER"],
    "push": int(os.environ["E2E_STRESS_RECORD_PUSH"]),
    "pull": int(os.environ["E2E_STRESS_RECORD_PULL"]),
    "manifest_entries": int(os.environ["E2E_STRESS_RECORD_MANIFEST_ENTRIES"]),
    "manifest_revision": os.environ.get("E2E_STRESS_RECORD_MANIFEST_REVISION", ""),
    "sync_revision": os.environ.get("E2E_STRESS_RECORD_SYNC_REVISION", ""),
    "op_id": os.environ["E2E_STRESS_RECORD_OP_ID"],
    "local_mirror_files": int(os.environ["E2E_STRESS_RECORD_LOCAL_FILES"]),
    "local_vs_server_before": json.loads(os.environ.get("E2E_STRESS_RECORD_DIFF_BEFORE", "[]")),
    "on_disk_changed": json.loads(os.environ.get("E2E_STRESS_RECORD_DISK_CHANGED", "[]")),
}
path.parent.mkdir(parents=True, exist_ok=True)
with path.open("a", encoding="utf-8") as f:
    f.write(json.dumps(record, separators=(",", ":")) + "\n")
PY
}

# SHA256 index of mirror-eligible files under physical sync repo (matches client walk rules).
e2e_mirror_sha256_index() {
  local sync_repo="$1"
  python3 - "$sync_repo" <<'PY'
import hashlib, json, pathlib, sys

repo = pathlib.Path(sys.argv[1])

def excluded(rel: str) -> bool:
    if rel == "workspace_store" or rel.startswith("workspace_store/"):
        return True
    if rel == "config.toml":
        return True
    if rel == "store/git_target":
        return True
    if rel == "store/git" or rel.startswith("store/git/"):
        return True
    return False

out = {}
stack = [pathlib.Path("")]
while stack:
    rel = stack.pop()
    abs_p = repo / rel
    for ent in sorted(abs_p.iterdir(), key=lambda e: e.name):
        if rel == pathlib.Path("") and ent.name == ".dojjo":
            continue
        child = rel / ent.name
        if ent.is_symlink():
            continue
        if ent.is_dir():
            stack.append(child)
            continue
        if not ent.is_file():
            raise SystemExit(f"unsupported type: {ent}")
        slash = child.as_posix()
        if excluded(slash):
            continue
        data = ent.read_bytes()
        out[slash] = {
            "size": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }
print(json.dumps(out, sort_keys=True))
PY
}

# Paths whose bytes changed on disk between two mirror indexes (JSON from e2e_mirror_sha256_index).
e2e_mirror_index_changed_paths() {
  local before_json="$1"
  local after_json="$2"
  python3 - "$before_json" "$after_json" <<'PY'
import json, sys

before = json.loads(sys.argv[1])
after = json.loads(sys.argv[2])
keys = sorted(set(before) | set(after))
for k in keys:
    if before.get(k) != after.get(k):
        print(k)
PY
}

# Paths where local sync repo differs from server manifest entries.
e2e_mirror_local_vs_server_diff() {
  local sync_repo="$1"
  local api_base="$2"
  local dojo_id="$3"
  python3 - "$sync_repo" "$api_base" "$dojo_id" <<'PY'
import hashlib, json, subprocess, sys, urllib.request

repo, api_base, dojo_id = sys.argv[1:4]
api_base = api_base.rstrip("/")
with urllib.request.urlopen(f"{api_base}/dojo/{dojo_id}/mirror/manifest") as r:
    man = json.load(r)
entries = {e["path"]: e for e in man["entries"]}

def excluded(rel: str) -> bool:
    if rel == "workspace_store" or rel.startswith("workspace_store/"):
        return True
    if rel == "config.toml":
        return True
    if rel == "store/git_target":
        return True
    if rel == "store/git" or rel.startswith("store/git/"):
        return True
    return False

import pathlib
repo = pathlib.Path(repo)
local = {}
stack = [pathlib.Path("")]
while stack:
    rel = stack.pop()
    abs_p = repo / rel
    for ent in sorted(abs_p.iterdir(), key=lambda e: e.name):
        if rel == pathlib.Path("") and ent.name == ".dojjo":
            continue
        child = rel / ent.name
        if ent.is_symlink():
            continue
        if ent.is_dir():
            stack.append(child)
            continue
        slash = child.as_posix()
        if excluded(slash):
            continue
        data = ent.read_bytes()
        local[slash] = hashlib.sha256(data).hexdigest()

for p, sha in sorted(local.items()):
    ent = entries.get(p)
    if ent is None or ent["sha256"] != sha:
        print(p)
for p in sorted(entries):
    if p not in local:
        print(p)
PY
}

# JJ root() uses an all-zero CommitId; git_backend reads it without a Git object (see
# jj_lib::git_backend::read_commit). Do not probe it with git cat-file.
e2e_is_jj_root_commit_id() {
  local cid="$1"
  [[ ${#cid} -eq 40 ]]
  [[ "$cid" =~ ^0+$ ]]
}

e2e_assert_clone_lacks_commit() {
  local clone_ws="$1"
  local commit_id="$2"
  if [[ -z "$clone_ws" || -z "$commit_id" ]]; then
    echo "e2e_assert_clone_lacks_commit: workspace and commit_id required" >&2
    return 1
  fi
  if (cd "$clone_ws" && "$JJ" show -r "$commit_id" >/dev/null 2>&1); then
    echo "clone must not have commit $commit_id before sync (bridge not isolated?)" >&2
    return 1
  fi
}

e2e_assert_jj_repo_usable() {
  local ws="$1"
  (cd "$ws" && "$JJ" log -n 2 >/dev/null)
  (cd "$ws" && "$JJ" st >/dev/null)
}

e2e_assert_git_dir_usable() {
  local git_dir="$1"
  "$GIT" -C "$git_dir" rev-parse --git-dir >/dev/null
  "$GIT" -C "$git_dir" fsck --no-progress >/dev/null 2>&1 || true
}

# Distinct physical sync hosts, then shared repo semantics (Git HEAD for non-colocated).
e2e_assert_peers_converged() {
  local origin_ws="$1"
  local clone_ws="$2"
  local layout="${3:-}"
  local sync_repo_a="$4"
  local sync_repo_b="$5"

  e2e_assert_distinct_paths "$sync_repo_a" "$sync_repo_b"

  if [[ "$layout" == "colocated" ]]; then
    (cd "$origin_ws" && "$JJ" git import)
    (cd "$clone_ws" && "$JJ" git import)
  fi

  if [[ "$layout" != "colocated" ]]; then
    local og cg
    og="$("$GIT" -C "$(e2e_resolve_git_dir_from_repo "${origin_ws}/.jj/repo")" rev-parse HEAD)"
    cg="$("$GIT" -C "$(e2e_resolve_git_dir_from_repo "${clone_ws}/.jj/repo")" rev-parse HEAD)"
    if [[ "$og" != "$cg" ]]; then
      echo "git HEAD differs after sync: origin=$og clone=$cg" >&2
      return 1
    fi
  fi
}

e2e_assert_manifest_no_git_objects() {
  local api_base="$1"
  local dojo_id="$2"
  curl -sf "${api_base}/dojo/${dojo_id}/mirror/manifest" | python3 -c '
import json, sys
data = json.load(sys.stdin)
bad = [
    e["path"] for e in data.get("entries", [])
    if e["path"] == "store/git" or e["path"].startswith("store/git/")
]
if bad:
    print("manifest lists store/git/* (git transport only):", file=sys.stderr)
    for p in bad[:20]:
        print(p, file=sys.stderr)
    sys.exit(1)
'
}

e2e_assert_manifest_no_workspace_store() {
  local api_base="$1"
  local dojo_id="$2"
  curl -sf "${api_base}/dojo/${dojo_id}/mirror/manifest" | python3 -c '
import json, sys
data = json.load(sys.stdin)
bad = [
    e["path"] for e in data.get("entries", [])
    if e["path"] == "workspace_store" or e["path"].startswith("workspace_store/")
]
if bad:
    print("manifest lists workspace_store/* (machine-local paths):", file=sys.stderr)
    for p in bad[:20]:
        print(p, file=sys.stderr)
    sys.exit(1)
'
}

e2e_assert_tus_rejects_git_object_path() {
  local api_base="$1"
  local dojo_id="$2"
  local origin
  origin="$(echo "$api_base" | sed -E 's#/api$##')"
  local meta_b64
  meta_b64="$(printf 'store/git/objects/ab/deadbeef' | base64 | tr -d '\n')"
  local headers
  headers="$(curl -si -X POST "${api_base}/dojo/${dojo_id}/uploads" \
    -H "Tus-Resumable: 1.0.0" \
    -H "Upload-Length: 1" \
    -H "Upload-Metadata: jj_rel_path ${meta_b64}")"
  local location
  location="$(echo "$headers" | tr -d '\r' | awk 'tolower($0) ~ /^location:/ { sub(/^[^:]+: /,""); print; exit }')"
  if [[ -z "$location" ]]; then
    echo "TUS create did not return Location" >&2
    return 1
  fi
  local upload_url="${origin}${location}"
  local status
  status="$(curl -s -o /dev/null -w "%{http_code}" \
    -X PATCH "$upload_url" \
    -H "Tus-Resumable: 1.0.0" \
    -H "Upload-Offset: 0" \
    -H "Content-Type: application/offset+octet-stream" \
    --data-binary 'x')"
  if [[ "$status" != "400" ]]; then
    echo "expected TUS finalize to reject store/git/objects path, got HTTP $status" >&2
    return 1
  fi
}

e2e_fetch_git_remote_url() {
  local api_base="$1"
  local dojo_id="$2"
  if [[ -z "$api_base" || -z "$dojo_id" ]]; then
    echo "e2e_fetch_git_remote_url: api_base and dojo_id required" >&2
    return 1
  fi
  curl -sf "${api_base}/dojo/${dojo_id}" | python3 -c '
import json, sys
url = json.load(sys.stdin).get("git_remote_url", "")
if not url:
    sys.exit("missing git_remote_url in GET /dojo response")
print(url)
'
}

# Smart HTTP Git: API must return http(s)://…/git/<dojo>.git and git ls-remote must succeed.
e2e_assert_git_http_ls_remote() {
  local url="$1"
  if [[ -z "$url" ]]; then
    echo "e2e_assert_git_http_ls_remote: url required" >&2
    return 1
  fi
  if [[ -z "${GIT:-}" ]]; then
    echo "e2e_assert_git_http_ls_remote: GIT must be set by caller" >&2
    return 1
  fi
  case "$url" in
    http://* | https://* ) ;;
    *)
      echo "expected http(s) git remote from sync-server, got: $url" >&2
      return 1
      ;;
  esac
  case "$url" in
    */git/*.git) ;;
    *)
      echo "expected …/git/<dojo-id>.git remote, got: $url" >&2
      return 1
      ;;
  esac
  if ! "$GIT" ls-remote "$url" >/dev/null 2>&1; then
    echo "git ls-remote failed for $url" >&2
    return 1
  fi
}

e2e_pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

# Sets DOJJO_API_BASE, DOJJO_PUBLIC_URL, DATABASE_URL, DOJJO_LISTEN; starts SERVER_PID in TMP.
e2e_start_server() {
  local tmp="$1"
  local db_suffix="${2:-}"
  E2E_PORT="$(e2e_pick_port)"
  export DATABASE_URL="sqlite:${tmp}/dojjo${db_suffix}.sqlite"
  export DOJJO_DATA_DIR="${tmp}/dojjo-data"
  export DOJJO_LISTEN="127.0.0.1:${E2E_PORT}"
  export DOJJO_PUBLIC_URL="http://127.0.0.1:${E2E_PORT}"
  export DOJJO_API_BASE="${DOJJO_PUBLIC_URL}/api"
  mkdir -p "$DOJJO_DATA_DIR"
  cd "$tmp"
  "$SYNC_SERVER" &
  SERVER_PID=$!
  local _
  for _ in $(seq 1 100); do
    if (echo >/dev/tcp/127.0.0.1/"${E2E_PORT}") 2>/dev/null; then
      break
    fi
    sleep 0.05
  done
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "sync-server exited unexpectedly" >&2
    return 1
  fi
}

e2e_stop_server() {
  if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  unset SERVER_PID
}
