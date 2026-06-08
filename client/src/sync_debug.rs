//! Optional NDJSON sync diagnostics on stderr (`DOJJO_SYNC_DEBUG=1`).

use std::collections::HashMap;
use std::io::Write;

/// `true` when `DOJJO_SYNC_DEBUG` is set to a non-empty value other than `0` / `false`.
pub fn enabled() -> bool {
    match std::env::var("DOJJO_SYNC_DEBUG") {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && v != "false"
        }
        Err(_) => false,
    }
}

/// Paths whose `(size, sha256)` differ between two mirror indexes (sorted).
pub fn index_changed_paths(
    before: &HashMap<String, (u64, String)>,
    after: &HashMap<String, (u64, String)>,
) -> Vec<String> {
    let mut out = Vec::new();
    let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
    for k in keys {
        if before.get(k) != after.get(k) {
            out.push(k.clone());
        }
    }
    out
}

#[derive(serde::Serialize)]
pub struct PullCandidate<'a> {
    pub path: &'a str,
    pub reason: &'a str,
    pub in_local_at_start: bool,
    pub local_sha: Option<&'a str>,
    pub server_sha: &'a str,
}

#[derive(serde::Serialize)]
pub struct PulledRecord {
    pub path: String,
    pub in_local_at_start: bool,
    pub server_sha: String,
}

#[derive(serde::Serialize)]
pub struct SyncDebugRecord<'a> {
    pub phase: &'a str,
    pub workspace_op_id: Option<&'a str>,
    pub man_revision: Option<&'a str>,
    pub manifest_entries: Option<usize>,
    pub git_skipped: Option<bool>,
    pub git_need_push: Option<bool>,
    pub git_need_fetch: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref_mismatch: Option<Vec<(String, String, String)>>,
    pub push_count: Option<usize>,
    pub pull_count: Option<usize>,
    pub pull_would_count: Option<usize>,
    pub paths_changed: Option<&'a [String]>,
    pub push_paths: Option<&'a [String]>,
    pub pull_paths: Option<&'a [String]>,
    pub pull_candidates: Option<&'a [PullCandidate<'a>]>,
    pub pulled_records: Option<&'a [PulledRecord]>,
    pub local_op_heads: Option<&'a [String]>,
    pub server_op_heads: Option<&'a [String]>,
    /// Keys in the mirror index whose path starts with this prefix (diagnostic).
    pub local_index_prefix_count: Option<usize>,
}

/// Count mirror-index keys under `prefix` (for idle-churn diagnosis).
pub fn index_prefix_count(index: &HashMap<String, (u64, String)>, prefix: &str) -> usize {
    assert!(!prefix.is_empty(), "prefix must not be empty");
    index.keys().filter(|k| k.starts_with(prefix)).count()
}

pub fn emit(record: &SyncDebugRecord<'_>) {
    if !enabled() {
        return;
    }
    let line = serde_json::to_string(record).expect("sync debug record must serialize");
    assert!(!line.is_empty(), "debug line must not be empty");
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "{line}").expect("write sync debug to stderr");
}

pub fn list_op_head_ids(jj_repo_folder: &std::path::Path) -> Vec<String> {
    let heads_dir = jj_repo_folder.join("op_heads").join("heads");
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&heads_dir) else {
        return out;
    };
    for ent in rd.flatten() {
        if ent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            if let Some(name) = ent.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}
