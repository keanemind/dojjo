#!/usr/bin/env bash
# Reproduce raw idle churn (mitigations off) and capture DOJJO_SYNC_DEBUG NDJSON.
#
# Env:
#   DOJJO_SYNC_INVESTIGATE=1  — force git push/fetch; no idle pull/push skip or prune
#   DOJJO_SYNC_DEBUG=1        — required (set by this script)
#   E2E_IDLE_SYNC_STRESS_ROUNDS — default 3 (short trace)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export DOJJO_SYNC_INVESTIGATE=1
export DOJJO_SYNC_DEBUG=1
export E2E_IDLE_SYNC_STRESS_ROUNDS="${E2E_IDLE_SYNC_STRESS_ROUNDS:-3}"
export E2E_IDLE_SYNC_EXPECT_QUIET=0

STRESS_OUT="$("${ROOT}/scripts/e2e-idle-sync-stress.sh" 2>&1)" || true
STRESS_EXIT=$?
echo "$STRESS_OUT"
DEBUG_LOG="$(echo "$STRESS_OUT" | sed -n 's/^idle-sync-stress: debug_ndjson=//p' | tail -1)"
if [[ -z "$DEBUG_LOG" || ! -f "$DEBUG_LOG" ]]; then
  echo "no debug log path in stress output" >&2
  exit 1
fi

echo ""
echo "=== idle-sync investigation summary (from $DEBUG_LOG) ==="
python3 - "$DEBUG_LOG" <<'PY'
import json, sys
from collections import Counter, defaultdict

path = sys.argv[1]
records = []
for line in open(path, encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    records.append(json.loads(line))

pull_phases = [r for r in records if r.get("phase") == "pull_analysis"]
print(f"debug lines: {len(records)}  pull_analysis phases: {len(pull_phases)}")

by_reason = Counter()
by_mitigation = Counter()
git_changed = Counter()
ref_mismatch_any = 0

for r in records:
    phase = r.get("phase")
    if phase == "start" and r.get("git_ref_mismatch"):
        ref_mismatch_any += 1
    for p in r.get("paths_changed") or []:
        if phase in ("after_git_push", "after_git_fetch"):
            prefix = p.split("/")[0] if "/" in p else p
            if p.startswith("store/extra/heads"):
                prefix = "store/extra/heads"
            elif p.startswith("op_heads/heads"):
                prefix = "op_heads/heads"
            git_changed[prefix] += 1
    for c in r.get("pull_candidates") or []:
        by_reason[c["reason"]] += 1
        if c.get("skipped_by_mitigation"):
            by_mitigation[c.get("mitigation") or "unknown"] += 1

print(f"syncs with git ref mismatch at start: {ref_mismatch_any}")
if git_changed:
    print("paths_changed counts by prefix (git legs):")
    for k, v in git_changed.most_common():
        print(f"  {k}: {v}")

if pull_phases:
    r0 = pull_phases[0]
    print(f"example pull_analysis: pull_would={r0.get('pull_would_count')} pull_actual={r0.get('pull_count')} investigate={r0.get('investigate')}")
    print(f"  workspace_op={ (r0.get('workspace_op_id') or '')[:16]}…")
    print(f"  local op_heads ({len(r0.get('local_op_heads') or [])}): {[x[:12]+'…' for x in (r0.get('local_op_heads') or [])[:5]]}")
    print(f"  server op_heads ({len(r0.get('server_op_heads') or [])}): {[x[:12]+'…' for x in (r0.get('server_op_heads') or [])[:5]]}")

print("pull candidate reasons (all phases):")
for k, v in by_reason.most_common():
    print(f"  {k}: {v}")

# Last churny pull_analysis with would > 0
for r in reversed(pull_phases):
    if (r.get("pull_would_count") or 0) > 0:
        print("last churny pull_analysis candidates (first 8):")
        for c in (r.get("pull_candidates") or [])[:8]:
            print(
                f"  {c['path'][:72]}{'…' if len(c['path'])>72 else ''}\n"
                f"    reason={c['reason']} local={ (c.get('local_sha') or '-')[:12]} server={c['server_sha'][:12]}"
            )
        break
PY

exit "$STRESS_EXIT"
