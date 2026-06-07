//! Pull mirror objects from the sync-server into a local `.jj/repo` directory.

use std::path::Path;

use anyhow::Context;
use dojjo_mirror::git_mirror_exclude::jj_repo_rel_excluded_from_git_file_mirror;
use dojjo_mirror::mirror_exclude::jj_repo_rel_excluded_from_mirror;
use reqwest::Client;

use crate::manifest::{fetch_manifest, get_mirror_object_bytes};

/// Reject obvious traversal / odd paths before writing under `repo_root`.
pub fn assert_safe_mirror_rel_path(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!path.is_empty(), "path must not be empty");
    anyhow::ensure!(!path.starts_with('/'), "path must be relative: {path:?}");
    anyhow::ensure!(
        !path.contains('\\'),
        "path must use forward slashes: {path:?}"
    );
    anyhow::ensure!(!path.contains(".."), "path must not contain '..': {path:?}");
    for seg in path.split('/') {
        anyhow::ensure!(!seg.is_empty(), "path has empty segment: {path:?}");
        anyhow::ensure!(seg != "." && seg != "..", "path has '.' segment: {path:?}");
    }
    Ok(())
}

pub async fn pull_mirror_into_repo(
    client: &Client,
    api_base: &str,
    dojo_id: &str,
    repo_root: &Path,
) -> anyhow::Result<usize> {
    assert!(repo_root.as_os_str().len() > 0, "repo_root must not be empty");
    anyhow::ensure!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let api_base = api_base.trim_end_matches('/');
    assert!(!api_base.is_empty(), "api_base must not be empty");

    let mut manifest = fetch_manifest(client, api_base, dojo_id).await?;
    if manifest.apply_order.is_empty() {
        return Ok(0);
    }

    let mut pulled = 0usize;
    for rel in manifest.apply_order.clone() {
        if jj_repo_rel_excluded_from_mirror(&rel) {
            continue;
        }
        if jj_repo_rel_excluded_from_git_file_mirror(&rel) {
            continue;
        }
        assert_safe_mirror_rel_path(&rel)?;
        let Some(bytes) =
            get_mirror_object_bytes(client, api_base, dojo_id, &rel, &mut manifest).await?
        else {
            continue;
        };

        let dest = repo_root.join(&rel);
        assert!(
            dest.starts_with(repo_root),
            "destination must stay under repo root"
        );
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        std::fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
        pulled += 1;
    }

    Ok(pulled)
}
