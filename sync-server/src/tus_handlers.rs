//! Axum glue for embedding [`tus_server::TusHandler`] under `/api/dojo/{dojo_id}/uploads`.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode},
};
use bytes::Bytes;
use tus_server::{
    CreateRequest, DeleteRequest, HeadRequest, PatchRequest, TusConfig, TusError, TusHandler,
    UploadId, validate_tus_resumable,
};

use crate::dojo_tus_store::{FsDojoUploadStore, MirrorFinalizeError};
use crate::{AppError, AppState};

fn header_str<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    let v = headers.get(name)?;
    let s = v.to_str().ok()?;
    assert!(!name.is_empty(), "header name must not be empty");
    Some(s)
}

fn apply_tus_headers(
    mut builder: axum::http::response::Builder,
    headers: Vec<(&'static str, String)>,
) -> axum::http::response::Builder {
    for (k, v) in headers {
        assert!(!k.is_empty(), "tus header name must not be non-empty");
        let name = HeaderName::from_static(k);
        let value = HeaderValue::from_str(&v).unwrap_or_else(|_| {
            panic!("tus response header value must be valid header bytes: {k}={v:?}")
        });
        builder = builder.header(name, value);
    }
    builder
}

fn mirror_finalize_error_response(err: MirrorFinalizeError) -> Response<Body> {
    let status = match &err {
        MirrorFinalizeError::GitStorePathRejected
        | MirrorFinalizeError::WorkspaceStorePathRejected
        | MirrorFinalizeError::MissingJjRelPath
        | MirrorFinalizeError::InvalidJjRelPath(_)
        | MirrorFinalizeError::LengthMismatch => StatusCode::BAD_REQUEST,
        MirrorFinalizeError::UploadNotFound => StatusCode::NOT_FOUND,
        MirrorFinalizeError::MissingDeclaredLength | MirrorFinalizeError::Io(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    assert!(
        status.as_u16() >= 400,
        "mirror finalize errors must be client or server errors"
    );
    Response::builder()
        .status(status)
        .body(Body::from(err.to_string()))
        .expect("mirror finalize error response must build")
}

fn tus_error_response(err: TusError) -> Response<Body> {
    let status = StatusCode::from_u16(err.status_code())
        .expect("tus error status codes must be valid HTTP status codes");
    let body = err.to_string();
    Response::builder()
        .status(status)
        .body(Body::from(body))
        .expect("error response must build")
}

fn new_tus_handler(dojo_root: std::path::PathBuf) -> TusHandler<FsDojoUploadStore> {
    assert!(
        !dojo_root.as_os_str().is_empty(),
        "dojo_root must not be empty"
    );
    let store = FsDojoUploadStore::new(dojo_root);
    let config = TusConfig::default();
    TusHandler::new(store, config)
}

pub(crate) async fn dojo_data_dir(
    state: &AppState,
    dojo_external_id: &str,
) -> Result<std::path::PathBuf, AppError> {
    assert!(!dojo_external_id.is_empty(), "dojo id must not be empty");
    let found = sqlx::query_scalar!(
        "SELECT id FROM dojos WHERE external_id = ?",
        dojo_external_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
    if found.is_none() {
        return Err(AppError::NotFound);
    }
    let dir = state.data_dir.join(dojo_external_id);
    assert!(
        dir.starts_with(&state.data_dir),
        "dojo dir must stay under data_dir"
    );
    Ok(dir)
}

/// Rejects path traversal or ambiguous dojo identifiers.
pub fn assert_safe_dojo_external_id(id: &str) -> Result<(), AppError> {
    assert!(!id.is_empty(), "dojo id must not be empty for safety check");
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(AppError::BadRequest);
    }
    Ok(())
}

pub async fn tus_uploads_options(
    State(state): State<Arc<AppState>>,
    Path(dojo_id): Path<String>,
) -> Result<Response<Body>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let _ = dojo_data_dir(&state, &dojo_id).await?;
    let handler = new_tus_handler(state.data_dir.join(&dojo_id));
    let resp = handler.handle_options();
    assert_eq!(resp.status_code(), 204);
    let builder = Response::builder().status(StatusCode::from_u16(resp.status_code()).unwrap());
    let builder = apply_tus_headers(builder, resp.headers());
    Ok(builder
        .body(Body::empty())
        .expect("OPTIONS response must build"))
}

pub async fn tus_uploads_post(
    State(state): State<Arc<AppState>>,
    Path(dojo_id): Path<String>,
    headers: HeaderMap,
    _body: Bytes,
) -> Result<Response<Body>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let dojo_dir = dojo_data_dir(&state, &dojo_id).await?;
    std::fs::create_dir_all(&dojo_dir).map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;

    let tus_version = header_str(&headers, tus_server::HEADER_TUS_RESUMABLE).unwrap_or("");
    if let Err(e) = validate_tus_resumable(tus_version) {
        return Ok(tus_error_response(e));
    }

    let upload_length = header_str(&headers, tus_server::HEADER_UPLOAD_LENGTH)
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|_| AppError::BadRequest)?;

    let defer_length = matches!(
        header_str(&headers, tus_server::HEADER_UPLOAD_DEFER_LENGTH),
        Some("1")
    );

    let metadata_raw = header_str(&headers, tus_server::HEADER_UPLOAD_METADATA).unwrap_or("");
    let metadata = tus_server::parse_metadata(metadata_raw).map_err(|_| AppError::BadRequest)?;

    let req = CreateRequest {
        upload_length,
        defer_length,
        metadata,
    };

    let handler = new_tus_handler(dojo_dir);
    let create = match handler.handle_create(req).await {
        Ok(r) => r,
        Err(e) => return Ok(tus_error_response(e)),
    };

    let location = format!("/api/dojo/{dojo_id}/uploads/{}", create.upload_id);
    assert!(
        location.starts_with("/api/dojo/"),
        "Location must be under /api/dojo/"
    );

    let mut tus_headers = create.headers();
    tus_headers.push((tus_server::HEADER_LOCATION, location));

    let builder = Response::builder().status(StatusCode::from_u16(create.status_code()).unwrap());
    let builder = apply_tus_headers(builder, tus_headers);
    Ok(builder
        .body(Body::empty())
        .expect("POST create response must build"))
}

pub async fn tus_upload_head(
    State(state): State<Arc<AppState>>,
    Path((dojo_id, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response<Body>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let dojo_dir = dojo_data_dir(&state, &dojo_id).await?;

    let tus_version = header_str(&headers, tus_server::HEADER_TUS_RESUMABLE).unwrap_or("");
    if let Err(e) = validate_tus_resumable(tus_version) {
        return Ok(tus_error_response(e));
    }

    assert!(
        !upload_id.is_empty(),
        "upload_id path segment must not be non-empty"
    );
    let req = HeadRequest {
        upload_id: upload_id.into(),
    };

    let handler = new_tus_handler(dojo_dir);
    let head = match handler.handle_head(req).await {
        Ok(r) => r,
        Err(e) => return Ok(tus_error_response(e)),
    };

    let builder = Response::builder().status(StatusCode::from_u16(head.status_code()).unwrap());
    let builder = apply_tus_headers(builder, head.headers());
    Ok(builder
        .body(Body::empty())
        .expect("HEAD response must build"))
}

pub async fn tus_upload_patch(
    State(state): State<Arc<AppState>>,
    Path((dojo_id, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let dojo_dir = dojo_data_dir(&state, &dojo_id).await?;

    let tus_version = header_str(&headers, tus_server::HEADER_TUS_RESUMABLE).unwrap_or("");
    if let Err(e) = validate_tus_resumable(tus_version) {
        return Ok(tus_error_response(e));
    }

    let offset: u64 = header_str(&headers, tus_server::HEADER_UPLOAD_OFFSET)
        .ok_or(AppError::BadRequest)?
        .parse()
        .map_err(|_| AppError::BadRequest)?;

    let content_type = header_str(&headers, axum::http::header::CONTENT_TYPE.as_str())
        .unwrap_or("")
        .to_string();

    assert!(
        !upload_id.is_empty(),
        "upload_id path segment must not be non-empty"
    );
    let upload_id_tus: UploadId = upload_id.clone().into();
    let req = PatchRequest {
        upload_id: upload_id_tus.clone(),
        upload_offset: offset,
        content_type,
        data: body,
    };

    let handler = new_tus_handler(dojo_dir.clone());
    let patch = match handler.handle_patch(req).await {
        Ok(r) => r,
        Err(e) => return Ok(tus_error_response(e)),
    };

    if patch.upload_complete {
        let store = FsDojoUploadStore::new(dojo_dir);
        if let Err(e) = store.finalize_into_mirror(&upload_id_tus).await {
            return Ok(mirror_finalize_error_response(e));
        }
    }

    let builder = Response::builder().status(StatusCode::from_u16(patch.status_code()).unwrap());
    let builder = apply_tus_headers(builder, patch.headers());
    Ok(builder
        .body(Body::empty())
        .expect("PATCH response must build"))
}

pub async fn tus_upload_delete(
    State(state): State<Arc<AppState>>,
    Path((dojo_id, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response<Body>, AppError> {
    assert_safe_dojo_external_id(&dojo_id)?;
    let dojo_dir = dojo_data_dir(&state, &dojo_id).await?;

    let tus_version = header_str(&headers, tus_server::HEADER_TUS_RESUMABLE).unwrap_or("");
    if let Err(e) = validate_tus_resumable(tus_version) {
        return Ok(tus_error_response(e));
    }

    assert!(
        !upload_id.is_empty(),
        "upload_id path segment must not be non-empty"
    );
    let req = DeleteRequest {
        upload_id: upload_id.into(),
    };

    let handler = new_tus_handler(dojo_dir);
    if let Err(e) = handler.handle_delete(req).await {
        return Ok(tus_error_response(e));
    }

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("DELETE response must build"))
}
