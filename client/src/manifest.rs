//! Fetch and parse mirror manifest JSON; pull mirror objects with 404 retry.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use dojjo_mirror::git_mirror_exclude::jj_repo_rel_git_transport_local_state;
use dojjo_mirror::mirror_exclude::jj_repo_rel_excluded_from_mirror;

/// Max manifest refetches when GET 404s (path removed by normalize or concurrent peer publish).
const MAX_MANIFEST_REFETCH_ON_404: u32 = 8;

#[derive(Debug, Clone, Deserialize, Eq, PartialEq)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub revision: String,
    pub entries: HashMap<String, ManifestEntry>,
    pub apply_order: Vec<String>,
}

pub async fn fetch_manifest(client: &Client, api_base: &str, dojo_id: &str) -> anyhow::Result<Manifest> {
    assert!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let url = format!(
        "{}/dojo/{dojo_id}/mirror/manifest",
        api_base.trim_end_matches('/')
    );
    let res = client.get(&url).send().await.context("GET manifest")?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("dojo not found (check dojo id and api base)");
    }
    let res = res.error_for_status().context("GET manifest status")?;
    let body: serde_json::Value = res.json().await.context("manifest json")?;
    let revision = body["revision"]
        .as_str()
        .context("manifest.revision")?
        .to_string();
    assert!(!revision.is_empty(), "revision must not be empty");

    let entries_arr = body["entries"].as_array().context("manifest.entries")?;
    let mut entries = HashMap::new();
    for e in entries_arr {
        let path = e["path"].as_str().context("entry.path")?.to_string();
        assert!(!path.is_empty(), "entry path must not be empty");
        let size = e["size"].as_u64().context("entry.size")?;
        let sha256 = e["sha256"].as_str().context("entry.sha256")?.to_string();
        assert_eq!(sha256.len(), 64, "sha256 must be 64 hex chars");
        entries.insert(
            path.clone(),
            ManifestEntry {
                path,
                size,
                sha256,
            },
        );
    }

    let apply_order: Vec<String> = body["apply_order"]
        .as_array()
        .context("manifest.apply_order")?
        .iter()
        .map(|v| v.as_str().context("apply_order string").map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        apply_order.len(),
        entries.len(),
        "apply_order must cover every entry exactly once"
    );
    for p in &apply_order {
        assert!(
            entries.contains_key(p),
            "apply_order path must exist in entries"
        );
    }

    Ok(Manifest {
        revision,
        entries,
        apply_order,
    })
}

/// Tell the sync-server to JJ-normalize `mirror/` after this peer finished `dev sync`.
pub async fn notify_sync_complete(
    client: &Client,
    api_base: &str,
    dojo_id: &str,
) -> anyhow::Result<()> {
    assert!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let url = format!(
        "{}/dojo/{dojo_id}/mirror/sync-complete",
        api_base.trim_end_matches('/')
    );
    let res = client
        .post(&url)
        .send()
        .await
        .context("POST mirror/sync-complete")?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("dojo not found (check dojo id and api base)");
    }
    res.error_for_status()
        .context("POST mirror/sync-complete status")?;
    Ok(())
}

/// GET one mirror object against the published manifest.
///
/// Refetches the manifest on 404 (path normalized away or manifest republished). After
/// `sync-complete`, manifest entries should match mirror disk; size/sha256 must agree.
///
/// Returns `Ok(None)` when the path is absent from the manifest after refetch.
pub async fn get_mirror_object_bytes(
    client: &Client,
    api_base: &str,
    dojo_id: &str,
    rel: &str,
    manifest: &mut Manifest,
) -> anyhow::Result<Option<Vec<u8>>> {
    assert!(!dojo_id.is_empty(), "dojo_id must not be empty");
    assert!(!rel.is_empty(), "rel must not be empty");
    let api_base = api_base.trim_end_matches('/');
    assert!(!api_base.is_empty(), "api_base must not be empty");

    let url = format!("{api_base}/dojo/{dojo_id}/mirror/{rel}");
    let mut refetches_on_404 = 0u32;

    loop {
        let ent = match manifest.entries.get(rel) {
            Some(e) => e,
            None => return Ok(None),
        };
        assert!(!ent.path.is_empty(), "manifest entry path must not be empty");
        assert_eq!(ent.sha256.len(), 64, "sha256 must be 64 hex chars");

        let res = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {rel}"))?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            refetches_on_404 += 1;
            if refetches_on_404 > MAX_MANIFEST_REFETCH_ON_404 {
                anyhow::bail!(
                    "GET {rel} returned 404 after {MAX_MANIFEST_REFETCH_ON_404} manifest refetches"
                );
            }
            *manifest = fetch_manifest(client, api_base, dojo_id).await?;
            continue;
        }

        let res = res
            .error_for_status()
            .with_context(|| format!("GET {rel} status"))?;
        let bytes = res
            .bytes()
            .await
            .with_context(|| format!("GET {rel} body"))?
            .to_vec();
        anyhow::ensure!(
            bytes.len() as u64 == ent.size,
            "length mismatch for {rel}"
        );
        let got_sha = hex::encode(Sha256::digest(&bytes));
        anyhow::ensure!(got_sha == ent.sha256, "sha256 mismatch for {rel}");
        return Ok(Some(bytes));
    }
}

/// Pull paths in `pull_set` following `manifest.apply_order`, writing under `dest_root`.
#[allow(dead_code)]
pub async fn pull_mirror_paths(
    client: &Client,
    api_base: &str,
    dojo_id: &str,
    dest_root: &Path,
    pull_set: &HashSet<String>,
    manifest: &mut Manifest,
) -> anyhow::Result<usize> {
    assert!(dest_root.as_os_str().len() > 0, "dest_root must not be empty");
    if pull_set.is_empty() {
        return Ok(0);
    }

    let mut pulled = 0usize;
    let apply_order = manifest.apply_order.clone();
    for rel in apply_order {
        if jj_repo_rel_excluded_from_mirror(&rel) {
            continue;
        }
        if jj_repo_rel_git_transport_local_state(&rel) {
            continue;
        }
        if !pull_set.contains(&rel) {
            continue;
        }
        let Some(bytes) = get_mirror_object_bytes(client, api_base, dojo_id, &rel, manifest).await?
        else {
            continue;
        };
        let dest = dest_root.join(&rel);
        assert!(dest.starts_with(dest_root), "dest must stay under dest_root");
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
        std::fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
        pulled += 1;
    }
    Ok(pulled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_refetch_constant_is_positive() {
        assert!(MAX_MANIFEST_REFETCH_ON_404 > 0);
    }
}
