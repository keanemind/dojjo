#!/usr/bin/env bash
# Controlled idle-sync experiments. Prints OBSERVED facts only (no mitigation).
# Requires: DOJJO_SYNC_DEBUG=1, built dojjo/sync-server, jj, git.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export DOJJO_SYNC_DEBUG=1
PROVE_OUT="${ROOT}/target/idle-sync-prove.ndjson"
mkdir -p "${ROOT}/target"
: >"$PROVE_OUT"

# shellcheck source=scripts/e2e-lib.sh
source "${ROOT}/scripts/e2e-lib.sh"

emit_obs() {
  python3 - "$PROVE_OUT" <<'PY' "$@"
import json, sys, time
path, experiment, observation, data = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
rec = {
    "t": time.time(),
    "experiment": experiment,
    "observation": observation,
    "data": json.loads(data) if data else {},
}
with open(path, "a", encoding="utf-8") as f:
    f.write(json.dumps(rec, separators=(",", ":")) + "\n")
print(f"OBS [{experiment}] {observation}")
if data:
    print(f"     {data}")
PY
}

append_sync_debug() {
  local label="$1"
  if [[ -f "${ROOT}/target/idle-sync-last-debug.ndjson" ]]; then
    python3 - "$PROVE_OUT" "$label" "${ROOT}/target/idle-sync-last-debug.ndjson" <<'PY'
import json, sys, time
prove, label, debug_path = sys.argv[1:4]
lines = [json.loads(l) for l in open(debug_path, encoding="utf-8") if l.strip()]
with open(prove, "a", encoding="utf-8") as f:
    f.write(json.dumps({
        "t": time.time(),
        "experiment": label,
        "observation": "sync_debug_trace",
        "data": {"lines": lines},
    }, separators=(",", ":")) + "\n")
PY
  fi
}

count_extra_heads() {
  local jj_repo_folder="$1"
  python3 - "$jj_repo_folder" <<'PY'
import json, pathlib, sys
repo = pathlib.Path(sys.argv[1]) / "store" / "extra" / "heads"
if not repo.is_dir():
    print(json.dumps({"files": 0, "symlinks": 0, "paths": []}))
    raise SystemExit
files, symlinks, paths = 0, 0, []
for p in sorted(repo.iterdir()):
    if p.is_symlink():
        symlinks += 1
        paths.append({"path": p.name, "kind": "symlink"})
    elif p.is_file():
        files += 1
        paths.append({"path": p.name, "kind": "file"})
print(json.dumps({"files": files, "symlinks": symlinks, "paths": paths[:20]}))
PY
}

snapshot_op_heads() {
  local jj_repo_folder="$1"
  python3 - "$jj_repo_folder" <<'PY'
import json, pathlib, sys
repo = pathlib.Path(sys.argv[1]) / "op_heads" / "heads"
ids = sorted(p.name for p in repo.iterdir() if p.is_file()) if repo.is_dir() else []
print(json.dumps(ids))
PY
}

snapshot_paths() {
  local jj_repo_folder="$1"
  python3 - "$jj_repo_folder" <<'PY'
import hashlib, json, pathlib, sys
repo = pathlib.Path(sys.argv[1])
# same exclusions as e2e_mirror_sha256_index (abbreviated)
def excluded(rel):
    if rel == "workspace_store" or rel.startswith("workspace_store/"): return True
    if rel == "config.toml": return True
    if rel == "store/git_target": return True
    if rel == "store/git" or rel.startswith("store/git/"): return True
    return False
out = {}
stack = [pathlib.Path("")]
while stack:
    rel = stack.pop()
    for ent in sorted((repo / rel).iterdir(), key=lambda e: e.name):
        if rel == pathlib.Path("") and ent.name == ".dojjo": continue
        child = rel / ent.name
        if ent.is_dir(): stack.append(child); continue
        if not ent.is_file(): continue
        slash = child.as_posix()
        if excluded(slash): continue
        data = ent.read_bytes()
        out[slash] = hashlib.sha256(data).hexdigest()
print(json.dumps(sorted(out.keys())))
PY
}

run_sync_debug() {
  local home="$1"
  local ws="$2"
  export E2E_SYNC_DEBUG_STDERR="${TMP}/sync-debug-$$.ndjson"
  : >"$E2E_SYNC_DEBUG_STDERR"
  e2e_dev_sync_metrics "$home" "$ws" >/dev/null
  cp "$E2E_SYNC_DEBUG_STDERR" "${ROOT}/target/idle-sync-last-debug.ndjson"
}

"${ROOT}/scripts/init-sync-server-db.sh"
cargo build -q --bin dojjo --bin sync-server
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
SYNC_SERVER="${TARGET_DIR}/debug/sync-server"
DOJJO="${TARGET_DIR}/debug/dojjo"
JJ="$(command -v jj)"
GIT="${GIT:-$(command -v git)}"
export SYNC_SERVER DOJJO JJ GIT

TMP="$(mktemp -d)"
e2e_init_peer_homes "$TMP"
cleanup() { e2e_stop_server; rm -rf "$TMP"; }
trap cleanup EXIT

e2e_start_server "$TMP" "-prove"
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
rm -rf "$CLONE"
mkdir -p "$CLONE"
e2e_cold_join "$DOJJO_HOME_B" "$DOJO_ID" "$CLONE" "clone" "$NEUTRAL"
DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_A" "$DOJO_ID")"
DEFAULT_WORKSPACE_JJ_REPO_FOLDER_B="$(e2e_default_workspace_jj_repo_folder "$DOJJO_HOME_B" "$DOJO_ID")"
GIT_DIR_A="$(e2e_resolve_git_dir_from_repo "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
GIT_URL="$(e2e_fetch_git_remote_url "$DOJJO_API_BASE" "$DOJO_ID")"

for _ in 1 2 3; do
  (cd "$ORIGIN" && e2e_dojjo_for_home "$DOJJO_HOME_A" dev sync >/dev/null)
  (cd "$CLONE" && e2e_dojjo_for_home "$DOJJO_HOME_B" dev sync >/dev/null)
done

OP_ID="$(cd "$ORIGIN" && "$JJ" op log -n 1 --no-graph -T 'id' | tr -d '[:space:]')"
emit_obs "setup" "converged" "{\"op_id\":\"$OP_ID\",\"jj_repo_folder_a\":\"$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A\"}"

# --- Experiment 1: A sync twice, no B, no jj between ---
emit_obs "exp1" "begin" "{\"sequence\":\"A_sync,A_sync_no_jj_between\"}"

run_sync_debug "$DOJJO_HOME_A" "$ORIGIN"
append_sync_debug "exp1_A1"
PATHS_A1="$(snapshot_paths "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
HEADS_A1="$(snapshot_op_heads "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
emit_obs "exp1" "after_A1" "{\"paths\":$PATHS_A1,\"op_heads\":$HEADS_A1}"

IDX_BETWEEN="$(e2e_mirror_sha256_index "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
emit_obs "exp1" "between_A1_A2_e2e_index" "$IDX_BETWEEN"

HEADS_BEFORE_JJ="$(count_extra_heads "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
OP_HEADS_BEFORE="$(snapshot_op_heads "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
(cd "$ORIGIN" && "$JJ" op log -n 1 --no-graph -T 'id' >/dev/null)
HEADS_AFTER_JJ="$(count_extra_heads "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
OP_HEADS_AFTER="$(snapshot_op_heads "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
IDX_AFTER_JJ="$(e2e_mirror_sha256_index "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
emit_obs "exp1" "jj_op_log_between_syncs" "{\"heads_before\":$HEADS_BEFORE_JJ,\"heads_after\":$HEADS_AFTER_JJ,\"op_heads_before\":$OP_HEADS_BEFORE,\"op_heads_after\":$OP_HEADS_AFTER}"
emit_obs "exp1" "after_jj_op_log_e2e_index" "$IDX_AFTER_JJ"

# No jj, no sleep, no B — immediate second sync
run_sync_debug "$DOJJO_HOME_A" "$ORIGIN"
append_sync_debug "exp1_A2"
PATHS_A2="$(snapshot_paths "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
HEADS_A2="$(snapshot_op_heads "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A")"
emit_obs "exp1" "after_A2" "{\"paths\":$PATHS_A2,\"op_heads\":$HEADS_A2}"

python3 - "$PROVE_OUT" <<'PY'
import json, sys
prove = sys.argv[1]
recs = [json.loads(l) for l in open(prove) if l.strip()]
def last(exp, obs):
    for r in reversed(recs):
        if r["experiment"] == exp and r["observation"] == obs:
            return r["data"]
    return {}
a1 = last("exp1", "after_A1")
a2 = last("exp1", "after_A2")
# parse traces
def pulls(label):
    for r in recs:
        if r["experiment"] != label or r["observation"] != "sync_debug_trace":
            continue
        for line in r["data"]["lines"]:
            if line.get("phase") == "done":
                return line.get("pull_paths") or [], line.get("pulled_records") or []
    return [], []
p1, pr1 = pulls("exp1_A1")
p2, pr2 = pulls("exp1_A2")
set1, set2 = set(p1), set(p2)
print("=== EXPERIMENT 1: A sync then A sync (no B, no jj) ===")
print(f"A1 pull_paths ({len(set1)}): {sorted(set1)[:5]}...")
print(f"A2 pull_paths ({len(set2)}): {sorted(set2)[:5]}...")
print(f"A2 repeats A1 path exactly: {sorted(set1 & set2)}")
print(f"A2 only new paths vs A1: {sorted(set2 - set1)}")
print(f"A2 only in A1 not pulled again: {sorted(set1 - set2)}")
for label, pr in [("A1", pr1), ("A2", pr2)]:
    for rec in pr:
        print(f"  {label} pulled {rec['path'][:60]}… in_local_at_start={rec.get('in_local_at_start')}")
heads1, heads2 = set(a1.get("op_heads", [])), set(a2.get("op_heads", []))
print(f"op_heads after A1: {len(heads1)} files")
print(f"op_heads after A2: {len(heads2)} files")
print(f"op_heads removed between A1 done and A2 done: {sorted(heads1 - heads2)}")
print(f"op_heads added between A1 and A2: {sorted(heads2 - heads1)}")
between = last("exp1", "between_A1_A2_e2e_index")
if between:
    print(f"between A1/A2: e2e index has {len(between)} mirror files")
    for r in recs:
        if r["experiment"] != "exp1_A2" or r["observation"] != "sync_debug_trace":
            continue
        for line in r["data"]["lines"]:
            if line.get("phase") in ("start", "pull_analysis"):
                print(f"  A2 {line['phase']}: local_index_prefix_count(store/extra/heads/)={line.get('local_index_prefix_count')}")
    for p in sorted(set1):
        print(f"  A1 pulled path in between-index: {p in between}")
jj = last("exp1", "jj_op_log_between_syncs")
if jj:
    hb, ha = jj.get("heads_before", {}), jj.get("heads_after", {})
    print(f"  between A1/A2: jj op log only — store/extra/heads files {hb.get('files')} -> {ha.get('files')}, symlinks {hb.get('symlinks')} -> {ha.get('symlinks')}")
    ob, oa = set(jj.get("op_heads_before", [])), set(jj.get("op_heads_after", []))
    print(f"  between A1/A2: jj op log only — op_heads files {len(ob)} -> {len(oa)}, removed={sorted(ob - oa)}, added={sorted(oa - ob)}")
idx_after_jj = last("exp1", "after_jj_op_log_e2e_index")
if idx_after_jj and set1:
    n = sum(1 for p in set1 if p in idx_after_jj)
    print(f"  A1 pulled paths still in e2e index after jj op log: {n}/{len(set1)}")
PY

# --- Experiment 2: op_heads between phases of ONE sync (no jj) ---
emit_obs "exp2" "begin" "{\"sequence\":\"single_A_sync_phase_snapshots\"}"
# Handled via debug trace phases start/after_git_push/after_git_fetch/done

run_sync_debug "$DOJJO_HOME_A" "$ORIGIN"
append_sync_debug "exp2_single"
python3 - "$PROVE_OUT" <<'PY'
import json, sys
prove = sys.argv[1]
recs = [json.loads(l) for l in open(prove) if l.strip()]
lines = None
for r in recs:
    if r["experiment"] == "exp2_single" and r["observation"] == "sync_debug_trace":
        lines = r["data"]["lines"]
        break
print("=== EXPERIMENT 2: op_heads across phases of one A sync (no jj between) ===")
for line in lines or []:
    ph = line.get("phase")
    if ph in ("start", "after_git_push", "after_git_fetch", "done"):
        heads = line.get("local_op_heads") or []
        print(f"  {ph}: op_heads count={len(heads)} ids={[h[:12]+'…' for h in heads[:4]]}")
PY

# --- Experiment 3: Git only when git_transport_need says idle ---
emit_obs "exp3" "begin" "{\"sequence\":\"git_push_fetch_index_diff_when_refs_aligned\"}"
NEED_PUSH="$(cd "$ORIGIN" && python3 -c 'print(0)' )"
# use dojjo's logic via debug start from a no-op sync probe - run sync debug and read start
run_sync_debug "$DOJJO_HOME_A" "$ORIGIN"
python3 - "$PROVE_OUT" "${ROOT}/target/idle-sync-last-debug.ndjson" "$DEFAULT_WORKSPACE_JJ_REPO_FOLDER_A" "$GIT_DIR_A" "$GIT_URL" <<'PY'
import json, subprocess, sys, hashlib, pathlib
prove, debug_path, jj_repo_folder, git_dir, git_url = sys.argv[1:6]
lines = [json.loads(l) for l in open(debug_path) if l.strip()]
start = next(l for l in lines if l["phase"] == "start")
print("=== EXPERIMENT 3: Git leg when manifest thinks refs aligned ===")
print(f"  git_need_push={start.get('git_need_push')} git_need_fetch={start.get('git_need_fetch')} git_skipped={start.get('git_skipped')}")
mm = start.get("git_ref_mismatch") or []
print(f"  ref mismatches at start: {len(mm)}")
for row in mm[:8]:
    print(f"    {row[0]} local={row[1]} remote={row[2]}")

def index(repo):
    def excluded(rel):
        if rel == "workspace_store" or rel.startswith("workspace_store/"): return True
        if rel == "config.toml": return True
        if rel == "store/git_target": return True
        if rel == "store/git" or rel.startswith("store/git/"): return True
        return False
    out = {}
    stack = [pathlib.Path("")]
    repo = pathlib.Path(repo)
    while stack:
        rel = stack.pop()
        for ent in sorted((repo / rel).iterdir(), key=lambda e: e.name):
            if rel == pathlib.Path("") and ent.name == ".dojjo": continue
            child = rel / ent.name
            if ent.is_dir(): stack.append(child); continue
            if not ent.is_file(): continue
            slash = child.as_posix()
            if excluded(slash): continue
            out[slash] = hashlib.sha256(ent.read_bytes()).hexdigest()
    return out

idx0 = index(jj_repo_folder)
subprocess.run(["git", "--git-dir", git_dir, "push", "--no-progress", "dojjo", "+refs/*:refs/*"],
               check=False, capture_output=True)
idx_push = index(jj_repo_folder)
subprocess.run(["git", "--git-dir", git_dir, "fetch", "--prune", "--no-progress", "dojjo",
                "+refs/heads/*:refs/remotes/dojjo/*", "+refs/tags/*:refs/tags/*", "+refs/jj/*:refs/jj/*"],
               check=False, capture_output=True)
idx_fetch = index(jj_repo_folder)
def diff(a,b):
    keys = sorted(set(a)|set(b))
    return [k for k in keys if a.get(k)!=b.get(k)]
dp, df = diff(idx0, idx_push), diff(idx_push, idx_fetch)
print(f"  OBSERVED paths changed by git push alone: {len(dp)}")
for p in dp[:15]: print(f"    push: {p}")
print(f"  OBSERVED paths changed by git fetch alone (after push): {len(df)}")
for p in df[:15]: print(f"    fetch: {p}")
# mirror paths only
def kind(p):
    if p.startswith("store/extra/heads/"): return "extra_heads"
    if p.startswith("op_heads/heads/"): return "op_heads"
    if p.startswith("store/git/"): return "store/git"
    return "other"
from collections import Counter
cp, cf = Counter(kind(p) for p in dp), Counter(kind(p) for p in df)
print(f"  push change kinds: {dict(cp)}")
print(f"  fetch change kinds: {dict(cf)}")
PY

# --- Experiment 4: A sync, B sync, A sync — same path pulled on A3? ---
emit_obs "exp4" "begin" "{\"sequence\":\"A,B,A\"}"
run_sync_debug "$DOJJO_HOME_A" "$ORIGIN"
append_sync_debug "exp4_A1"
run_sync_debug "$DOJJO_HOME_B" "$CLONE"
append_sync_debug "exp4_B"
MAN_AFTER_B="$(curl -sf "${DOJJO_API_BASE}/dojo/${DOJO_ID}/mirror/manifest" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d["entries"]))')"
run_sync_debug "$DOJJO_HOME_A" "$ORIGIN"
append_sync_debug "exp4_A2"
python3 - "$PROVE_OUT" "$MAN_AFTER_B" <<'PY'
import json, sys
prove = sys.argv[1]
man_entries = int(sys.argv[2])
def pulls(label):
    for r in json.load(open(prove)) if False else [json.loads(l) for l in open(prove)]:
        pass
recs = [json.loads(l) for l in open(prove) if l.strip()]
def pull_paths(label):
    for r in recs:
        if r["experiment"] == label and r["observation"] == "sync_debug_trace":
            for line in r["data"]["lines"]:
                if line["phase"] == "done":
                    return set(line.get("pull_paths") or [])
    return set()
a1, b, a2 = pull_paths("exp4_A1"), pull_paths("exp4_B"), pull_paths("exp4_A2")
print("=== EXPERIMENT 4: A, B, A (idle alternation) ===")
print(f"  manifest entries after B sync: {man_entries}")
print(f"  A1 pull count={len(a1)} B pull count={len(b)} A2 pull count={len(a2)}")
print(f"  A2 ∩ A1 (same path pulled on both A syncs): {sorted(a2 & a1)}")
print(f"  A2 only (not in A1): {sorted(a2 - a1)[:8]}…")
print(f"  A2 ∩ B (paths A re-pulls that B also pulled): {sorted(a2 & b)[:8]}…")
# classify A2
for p in sorted(a2):
    tag = []
    if p in a1: tag.append("repeat_from_A1")
    if p in b: tag.append("also_in_B")
    if p.startswith("store/extra/heads/"): tag.append("extra_head")
    if p.startswith("op_heads/heads/"): tag.append("op_head")
    print(f"    A2 pull: {p[:70]}… {' '.join(tag)}")
PY

echo ""
echo "Full prove log: $PROVE_OUT"
