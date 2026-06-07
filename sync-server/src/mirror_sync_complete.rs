//! `POST /api/dojo/{dojo_id}/mirror/sync-complete` — JJ-normalize server mirror after client sync.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tracing::info;

use crate::dojo_tus_store::FsDojoUploadStore;
use crate::mirror_normalize::MirrorNormalizeError;
use crate::tus_handlers::{assert_safe_dojo_external_id, dojo_data_dir};
use crate::{AppError, AppState};

fn normalize_error_response(err: MirrorNormalizeError) -> Response {
    tracing::error!("mirror normalize failed: {err}");
    AppError::Internal(anyhow::anyhow!("{err}")).into_response()
}

pub async fn post_mirror_sync_complete(
    State(state): State<Arc<AppState>>,
    Path(dojo_id): Path<String>,
) -> Result<Response, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let dojo_dir = dojo_data_dir(&state, &dojo_id).await?;
    assert!(dojo_dir.is_dir(), "dojo dir must exist after lookup");

    let _dojo_io = state.dojo_io_locks.lock(&dojo_id).await;
    let store = FsDojoUploadStore::new(dojo_dir.clone());
    match store.normalize_mirror_after_sync().await {
        Ok(()) => {
            let published = crate::mirror_manifest::publish_mirror_manifest(&dojo_dir)
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("publish manifest: {e}")))?;
            info!(
                dojo_id = %dojo_id,
                revision = %published.revision,
                entries = published.entries.len(),
                "mirror normalized and manifest published after sync-complete"
            );
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(e) => Ok(normalize_error_response(e)),
    }
}
