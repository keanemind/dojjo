//! JSON manifest of all regular files under `{dojo}/mirror/`, with a stable `revision` for caching.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, Response, StatusCode, header},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::dojo_tus_store::MIRROR_DIR;
use crate::mirror_get::MAX_MIRROR_BLOB_BYTES;
use dojjo_mirror::git_mirror_exclude::jj_repo_rel_excluded_from_git_file_mirror;
use dojjo_mirror::mirror_apply_order::compute_mirror_apply_order;
use dojjo_mirror::mirror_exclude::jj_repo_rel_excluded_from_mirror;
use dojjo_mirror::mirror_path::{is_safe_mirror_path_segment, parse_safe_mirror_rel};
use crate::tus_handlers::{assert_safe_dojo_external_id, dojo_data_dir};
use crate::{AppError, AppState};

/// Upper bound on manifest entries (avoids runaway walks).
const MAX_MANIFEST_ENTRIES: usize = 100_000;

/// Published mirror snapshot at `{dojo_root}/mirror_manifest.json` (written at `sync-complete`).
pub const PUBLISHED_MANIFEST_FILE: &str = "mirror_manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestBody {
    pub revision: String,
    pub entries: Vec<ManifestEntry>,
    /// Safe **pull** order: `store` → `views` → `op_store/type` → **operations (DAG)** → misc → `index` → `submodules` → `op_heads`.
    pub apply_order: Vec<String>,
}

fn path_to_slash(rel: &Path) -> String {
    assert!(
        rel.components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "rel path must contain only normal components"
    );
    rel.iter()
        .map(|c| {
            c.to_str()
                .expect("mirror walk only allows UTF-8 segment characters")
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn compute_revision(entries: &[ManifestEntry]) -> String {
    let mut sorted: Vec<&ManifestEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = Sha256::new();
    for e in sorted {
        assert!(!e.path.is_empty(), "manifest path must not be empty");
        assert_eq!(e.sha256.len(), 64, "sha256 hex must be 64 chars");
        hasher.update(e.path.as_bytes());
        hasher.update([0u8]);
        hasher.update(e.size.to_be_bytes());
        hasher.update([0u8]);
        hasher.update(e.sha256.as_bytes());
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
}

fn if_none_match_includes_revision(if_none_match: &str, revision: &str) -> bool {
    assert!(!revision.is_empty(), "revision must not be empty");
    for part in if_none_match.split(',') {
        let p = part.trim();
        let p = p.strip_prefix("W/").unwrap_or(p).trim();
        let p = p
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(p);
        if p == revision {
            return true;
        }
    }
    false
}

/// Walk `mirror_root` and return one manifest row per regular file (sorted only in [`compute_revision`]).
pub async fn scan_mirror_tree(mirror_root: &Path) -> Result<Vec<ManifestEntry>, AppError> {
    assert!(
        mirror_root.as_os_str().len() > 0,
        "mirror_root must not be empty"
    );

    if !mirror_root.exists() {
        return Ok(Vec::new());
    }

    let root_meta = fs::metadata(mirror_root)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if !root_meta.is_dir() {
        return Err(AppError::BadRequest);
    }
    assert!(root_meta.is_dir(), "root must be a directory");

    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    let mut seen = 0usize;

    while let Some(rel) = stack.pop() {
        let abs = mirror_root.join(&rel);
        assert!(
            abs.starts_with(mirror_root),
            "walk must stay under mirror root"
        );
        let mut rd = fs::read_dir(&abs)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
        while let Some(ent) = rd
            .next_entry()
            .await
            .map_err(|e| AppError::Internal(e.into()))?
        {
            seen += 1;
            if seen > MAX_MANIFEST_ENTRIES {
                return Err(AppError::BadRequest);
            }

            let ft = ent
                .file_type()
                .await
                .map_err(|e| AppError::Internal(e.into()))?;
            if ft.is_symlink() {
                return Err(AppError::BadRequest);
            }

            let name = ent.file_name();
            let seg = name.to_str().ok_or(AppError::BadRequest)?;
            if !is_safe_mirror_path_segment(seg) {
                return Err(AppError::BadRequest);
            }

            let mut child = rel.clone();
            child.push(seg);

            if ft.is_file() {
                let meta = ent
                    .metadata()
                    .await
                    .map_err(|e| AppError::Internal(e.into()))?;
                let len = meta.len();
                if len > MAX_MIRROR_BLOB_BYTES {
                    return Err(AppError::BadRequest);
                }
                assert!(meta.is_file(), "dir entry marked file must be a file");

                let full = mirror_root.join(&child);
                let bytes = fs::read(&full)
                    .await
                    .map_err(|e| AppError::Internal(e.into()))?;
                assert_eq!(
                    bytes.len() as u64,
                    len,
                    "read length must match directory entry metadata"
                );

                let path_str = path_to_slash(&child);
                parse_safe_mirror_rel(&path_str).map_err(|_| AppError::BadRequest)?;
                if jj_repo_rel_excluded_from_mirror(&path_str) {
                    continue;
                }
                if jj_repo_rel_excluded_from_git_file_mirror(&path_str) {
                    continue;
                }

                let sha256 = hex::encode(Sha256::digest(&bytes));
                assert_eq!(sha256.len(), 64, "sha256 hex length invariant");

                out.push(ManifestEntry {
                    path: path_str,
                    size: len,
                    sha256,
                });
            } else if ft.is_dir() {
                let dir_slash = path_to_slash(&child);
                if jj_repo_rel_excluded_from_mirror(&dir_slash) {
                    continue;
                }
                if jj_repo_rel_excluded_from_git_file_mirror(&dir_slash) {
                    continue;
                }
                stack.push(child);
            } else {
                return Err(AppError::BadRequest);
            }
        }
    }

    Ok(out)
}

/// Build manifest JSON body from a post-normalize scan of `mirror/`.
pub async fn build_manifest_body(mirror_root: &Path) -> Result<ManifestBody, AppError> {
    assert!(
        mirror_root.as_os_str().len() > 0,
        "mirror_root must not be empty"
    );
    let entries = scan_mirror_tree(mirror_root).await?;
    let revision = compute_revision(&entries);
    assert!(!revision.is_empty(), "revision hash must not be empty");

    let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    let apply_order = compute_mirror_apply_order(mirror_root, &paths).map_err(|e| {
        if e.kind() == std::io::ErrorKind::InvalidData {
            AppError::BadRequest
        } else {
            AppError::Internal(e.into())
        }
    })?;
    assert_eq!(
        apply_order.len(),
        paths.len(),
        "apply_order must list every mirrored path exactly once"
    );

    Ok(ManifestBody {
        revision,
        entries,
        apply_order,
    })
}

fn validate_manifest_body(body: &ManifestBody) -> Result<(), AppError> {
    assert!(!body.revision.is_empty(), "revision must not be empty");
    if body.apply_order.len() != body.entries.len() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "apply_order length {} must match entries length {}",
            body.apply_order.len(),
            body.entries.len()
        )));
    }
    for p in &body.apply_order {
        let found = body.entries.iter().any(|e| e.path == *p);
        if !found {
            return Err(AppError::Internal(anyhow::anyhow!(
                "apply_order path {p:?} missing from entries"
            )));
        }
    }
    Ok(())
}

pub fn published_manifest_path(dojo_dir: &Path) -> PathBuf {
    assert!(dojo_dir.as_os_str().len() > 0, "dojo_dir must not be empty");
    dojo_dir.join(PUBLISHED_MANIFEST_FILE)
}

/// Atomically write the published manifest after `sync-complete` normalize.
pub async fn publish_mirror_manifest(dojo_dir: &Path) -> Result<ManifestBody, AppError> {
    assert!(dojo_dir.as_os_str().len() > 0, "dojo_dir must not be empty");
    let mirror_root = dojo_dir.join(MIRROR_DIR);
    let body = build_manifest_body(&mirror_root).await?;
    validate_manifest_body(&body)?;

    let path = published_manifest_path(dojo_dir);
    let tmp = dojo_dir.join(format!("{PUBLISHED_MANIFEST_FILE}.tmp"));
    assert!(tmp.starts_with(dojo_dir), "tmp path must stay under dojo dir");

    let json = serde_json::to_vec(&body).map_err(|e| AppError::Internal(e.into()))?;
    assert!(!json.is_empty(), "manifest json must not be empty");

    fs::write(&tmp, &json)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    fs::rename(&tmp, &path)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    Ok(body)
}

async fn read_published_manifest(dojo_dir: &Path) -> Result<Option<ManifestBody>, AppError> {
    assert!(dojo_dir.as_os_str().len() > 0, "dojo_dir must not be empty");
    let path = published_manifest_path(dojo_dir);
    let bytes = match fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AppError::Internal(e.into())),
    };
    assert!(!bytes.is_empty(), "published manifest must not be empty");
    let body: ManifestBody =
        serde_json::from_slice(&bytes).map_err(|e| AppError::Internal(e.into()))?;
    validate_manifest_body(&body)?;
    Ok(Some(body))
}

fn manifest_response(body: &ManifestBody) -> Result<Response<Body>, AppError> {
    assert!(!body.revision.is_empty(), "revision must not be empty");
    let json = serde_json::to_vec(body).map_err(|e| AppError::Internal(e.into()))?;
    assert!(!json.is_empty(), "manifest json must not be empty");

    let etag = format!("\"{}\"", body.revision);
    let etag_val =
        axum::http::HeaderValue::from_str(&etag).expect("revision hex must be a valid etag token");

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag_val)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json))
        .expect("manifest response must build"))
}

pub async fn get_mirror_manifest(
    State(state): State<Arc<AppState>>,
    AxumPath(dojo_id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response<Body>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let dojo_dir = dojo_data_dir(&state, &dojo_id).await?;
    let mirror_root = dojo_dir.join(MIRROR_DIR);

    let body = if let Some(published) = read_published_manifest(&dojo_dir).await? {
        published
    } else {
        build_manifest_body(&mirror_root).await?
    };
    assert!(!body.revision.is_empty(), "revision hash must not be empty");

    if let Some(raw) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if if_none_match_includes_revision(raw, &body.revision) {
            let etag = format!("\"{}\"", body.revision);
            let etag_val = axum::http::HeaderValue::from_str(&etag)
                .expect("revision hex must be a valid etag token");
            return Ok(Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, etag_val)
                .body(Body::empty())
                .expect("304 response must build"));
        }
    }

    manifest_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn scan_empty_mirror() {
        let dir = tempdir().unwrap();
        let mirror = dir.path().join("mirror");
        std::fs::create_dir_all(&mirror).unwrap();
        let entries = scan_mirror_tree(&mirror).await.unwrap();
        assert!(entries.is_empty());
        assert_eq!(compute_revision(&entries), compute_revision(&[]));
    }

    #[tokio::test]
    async fn scan_one_file_hashes() {
        let dir = tempdir().unwrap();
        let mirror = dir.path().join("mirror");
        std::fs::create_dir_all(mirror.join("a")).unwrap();
        std::fs::write(mirror.join("a").join("b.txt"), b"xy").unwrap();
        let entries = scan_mirror_tree(&mirror).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a/b.txt");
        assert_eq!(entries[0].size, 2);
        assert_eq!(entries[0].sha256, hex::encode(Sha256::digest(b"xy")));
    }

    #[test]
    fn if_none_match_parses_quotes() {
        let rev = "abc";
        assert!(if_none_match_includes_revision("\"abc\"", rev));
        assert!(if_none_match_includes_revision("W/\"abc\"", rev));
        assert!(!if_none_match_includes_revision("\"ab\"", rev));
    }

    #[tokio::test]
    async fn publish_then_read_returns_published_not_live() {
        let dir = tempdir().unwrap();
        let dojo = dir.path();
        let mirror = dojo.join("mirror");
        std::fs::create_dir_all(mirror.join("a")).unwrap();
        std::fs::write(mirror.join("a").join("one.txt"), b"1").unwrap();

        let published = publish_mirror_manifest(dojo).await.unwrap();
        assert_eq!(published.entries.len(), 1);
        assert_eq!(published.entries[0].path, "a/one.txt");

        std::fs::write(mirror.join("a").join("two.txt"), b"2").unwrap();
        let read = read_published_manifest(dojo).await.unwrap().unwrap();
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.revision, published.revision);

        let live = build_manifest_body(&mirror).await.unwrap();
        assert_eq!(live.entries.len(), 2);
    }
}
