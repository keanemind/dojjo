//! Download finalized blobs from `{dojo}/mirror/<jj_rel_path>` (same rules as TUS `jj_rel_path`).

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{Response, StatusCode, header},
};
use tokio::fs;

use crate::dojo_tus_store::MIRROR_DIR;
use dojjo_mirror::git_mirror_exclude::jj_repo_rel_excluded_from_git_file_mirror;
use dojjo_mirror::mirror_exclude::jj_repo_rel_excluded_from_mirror;
use dojjo_mirror::mirror_path::parse_safe_mirror_rel;
use crate::tus_handlers::{assert_safe_dojo_external_id, dojo_data_dir};
use crate::{AppError, AppState};

/// Hard cap for a single mirror blob (GET body and manifest per-file read).
pub(crate) const MAX_MIRROR_BLOB_BYTES: u64 = 64 * 1024 * 1024;

pub async fn get_mirror_object(
    State(state): State<Arc<AppState>>,
    Path((dojo_id, jj_path)): Path<(String, String)>,
) -> Result<Response<Body>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let dojo_dir = dojo_data_dir(&state, &dojo_id).await?;

    let rel = parse_safe_mirror_rel(&jj_path).map_err(|_| AppError::BadRequest)?;
    let rel_slash = rel
        .iter()
        .map(|c| {
            c.to_str()
                .expect("parse_safe_mirror_rel only yields UTF-8 segments")
        })
        .collect::<Vec<_>>()
        .join("/");
    assert!(!rel_slash.is_empty(), "non-empty rel must stringify");
    if jj_repo_rel_excluded_from_mirror(&rel_slash) {
        return Err(AppError::BadRequest);
    }
    if jj_repo_rel_excluded_from_git_file_mirror(&rel_slash) {
        return Err(AppError::BadRequest);
    }
    let mirror_root = dojo_dir.join(MIRROR_DIR);
    let full_path = mirror_root.join(&rel);
    assert!(
        full_path.starts_with(&mirror_root),
        "resolved path must stay under mirror root"
    );

    let meta = fs::metadata(&full_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound
        } else {
            AppError::Internal(e.into())
        }
    })?;
    assert!(meta.is_file(), "mirror GET only supports regular files");
    let len = meta.len();
    if len > MAX_MIRROR_BLOB_BYTES {
        return Err(AppError::BadRequest);
    }

    let bytes = fs::read(&full_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound
        } else {
            AppError::Internal(e.into())
        }
    })?;
    assert_eq!(
        bytes.len() as u64,
        len,
        "read size must match file metadata"
    );

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(bytes))
        .expect("mirror GET response must build"))
}
