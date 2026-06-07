//! `GET /api/dojo/{id}/status` — server-side dojo storage diagnostics for clients.

use std::path::Path;
use std::sync::Arc;

use axum::{Json, extract::Path as AxumPath, extract::State};

use crate::dojo_tus_store::MIRROR_DIR;
use crate::git_remote::{self, bare_repo_path};
use crate::tus_handlers::{assert_safe_dojo_external_id, dojo_data_dir};
use crate::{AppError, AppState};

#[derive(Debug, Clone, utoipa::ToSchema, serde::Serialize, Eq, PartialEq)]
pub struct DojoStatus {
    pub id: String,
    pub registered_in_db: bool,
    pub data_dir_exists: bool,
    pub bare_git_exists: bool,
    pub bare_receivepack_enabled: Option<bool>,
    pub mirror_dir_exists: bool,
    pub mirror_has_files: bool,
    pub public_url_configured: bool,
    /// `http` when [`DOJJO_PUBLIC_URL`] is set; `file` for local `file://` remotes.
    pub git_remote_mode: String,
    /// Present when `bare_git_exists` (same rules as `GET /dojo/{id}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_remote_url: Option<String>,
}

/// Whether HTTP push (receive-pack) is allowed for this bare repo.
///
/// `git http-backend` enables receive-pack for authenticated clients by default.
/// Dojjo always sets `REMOTE_USER` (see `git_http::GIT_HTTP_REMOTE_USER`), so an unset
/// `http.receivepack` is fine. Only an explicit `http.receivepack = false` disables push.
async fn bare_receivepack_enabled(bare: &Path) -> Result<Option<bool>, AppError> {
    assert!(bare.as_os_str().len() > 0, "bare path must not be empty");
    if !bare.is_dir() {
        return Ok(None);
    }
    let canon = bare
        .canonicalize()
        .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
    let out = tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(&canon)
        .args(["config", "--get", "http.receivepack"])
        .output()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if !out.status.success() {
        return Ok(Some(true));
    }
    let v = String::from_utf8_lossy(&out.stdout);
    let v = v.trim();
    Ok(Some(v != "false"))
}

async fn mirror_has_any_file(mirror: &Path) -> bool {
    assert!(mirror.as_os_str().len() > 0, "mirror path must not be empty");
    let mut rd = match tokio::fs::read_dir(mirror).await {
        Ok(r) => r,
        Err(_) => return false,
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let Ok(ft) = ent.file_type().await else {
            continue;
        };
        if ft.is_file() {
            return true;
        }
        if ft.is_dir() {
            let sub = ent.path();
            let mut sub_rd = match tokio::fs::read_dir(&sub).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Ok(Some(sub_ent)) = sub_rd.next_entry().await {
                if sub_ent.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
                    return true;
                }
            }
        }
    }
    false
}

#[utoipa::path(
    get,
    path = "/dojo/{id}/status",
    responses((status = OK, body = DojoStatus)),
    params(("id" = String, Path, description = "Dojo external id"))
)]
pub async fn get_dojo_status(
    State(state): State<Arc<AppState>>,
    AxumPath(dojo_id): AxumPath<String>,
) -> Result<Json<DojoStatus>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;

    let registered_in_db = match dojo_data_dir(&state, &dojo_id).await {
        Ok(_) => true,
        Err(AppError::NotFound) => false,
        Err(e) => return Err(e),
    };

    let data_dir = state.data_dir.join(&dojo_id);
    assert!(
        data_dir.starts_with(&state.data_dir),
        "dojo data dir must stay under data_dir"
    );
    let data_dir_exists = data_dir.is_dir();

    let bare = bare_repo_path(&state.data_dir, &dojo_id)?;
    let bare_git_exists = bare.is_dir();

    let bare_receivepack_enabled = if bare_git_exists {
        bare_receivepack_enabled(&bare).await?
    } else {
        None
    };

    let mirror = data_dir.join(MIRROR_DIR);
    let mirror_dir_exists = mirror.is_dir();
    let mirror_has_files = if mirror_dir_exists {
        mirror_has_any_file(&mirror).await
    } else {
        false
    };

    let public_url_configured = state.public_url.is_some();
    let git_remote_mode = if public_url_configured {
        "http".to_string()
    } else {
        "file".to_string()
    };

    let git_remote_url = if bare_git_exists {
        let url = git_remote::git_remote_url_for_dojo(
            &state.data_dir,
            &dojo_id,
            state.public_url.as_ref(),
        )?;
        assert!(!url.is_empty(), "git_remote_url must not be empty");
        Some(url)
    } else {
        None
    };

    Ok(Json(DojoStatus {
        id: dojo_id,
        registered_in_db,
        data_dir_exists,
        bare_git_exists,
        bare_receivepack_enabled,
        mirror_dir_exists,
        mirror_has_files,
        public_url_configured,
        git_remote_mode,
        git_remote_url,
    }))
}
