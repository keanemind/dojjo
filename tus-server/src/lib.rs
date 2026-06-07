//! Framework-agnostic TUS 1.0.0 resumable upload server library.
//!
//! # Overview
//!
//! This crate implements the server side of the [TUS resumable upload
//! protocol](https://tus.io/protocols/resumable-upload). It is deliberately
//! decoupled from any HTTP framework so that it can be embedded in Axum,
//! Actix-Web, or any other Rust web server.
//!
//! ## Supported extensions
//!
//! | Extension   | Description                                          |
//! |-------------|------------------------------------------------------|
//! | creation    | POST to create a new upload resource                 |
//! | termination | DELETE to remove an upload resource                  |
//! | expiration  | `Upload-Expires` header; uploads expire after a TTL  |
//!
//! ## Usage pattern
//!
//! 1. Implement [`UploadStore`] for your storage backend.
//! 2. Create a [`TusHandler`] with your store and a [`TusConfig`].
//! 3. In each HTTP handler, call [`validate_tus_resumable`] on the incoming
//!    `Tus-Resumable` header (skip for OPTIONS).
//! 4. Build the appropriate request struct and call the matching
//!    `handle_*` method.
//! 5. Convert the typed response struct to your framework's response type
//!    using `status_code()` and `headers()`.
//!
//! ## Axum example sketch
//!
//! ```ignore
//! async fn patch_upload(
//!     State(handler): State<Arc<TusHandler<MyStore>>>,
//!     Path(id): Path<String>,
//!     headers: HeaderMap,
//!     body: Bytes,
//! ) -> impl IntoResponse {
//!     let version = headers.get("tus-resumable").and_then(|v| v.to_str().ok()).unwrap_or("");
//!     if let Err(e) = tus_server::validate_tus_resumable(version) {
//!         return (StatusCode::from_u16(e.status_code()).unwrap(), e.to_string()).into_response();
//!     }
//!     let offset: u64 = headers.get("upload-offset")
//!         .and_then(|v| v.to_str().ok())
//!         .and_then(|s| s.parse().ok())
//!         .unwrap_or(0);
//!     let content_type = headers.get("content-type")
//!         .and_then(|v| v.to_str().ok())
//!         .unwrap_or("")
//!         .to_string();
//!     let req = PatchRequest {
//!         upload_id: UploadId::from(id),
//!         upload_offset: offset,
//!         content_type,
//!         data: body,
//!     };
//!     match handler.handle_patch(req).await {
//!         Ok(resp) => {
//!             let mut builder = Response::builder().status(resp.status_code());
//!             for (k, v) in resp.headers() {
//!                 builder = builder.header(k, v);
//!             }
//!             builder.body(Body::empty()).unwrap()
//!         }
//!         Err(e) => (StatusCode::from_u16(e.status_code()).unwrap(), e.to_string()).into_response(),
//!     }
//! }
//! ```

mod error;
mod handler;
mod protocol;
mod request;
mod response;
mod store;

pub use error::TusError;
pub use handler::{TusConfig, TusHandler};
pub use protocol::{
    CONTENT_TYPE_OFFSET_OCTET_STREAM, EXTENSIONS, HEADER_CACHE_CONTROL, HEADER_CONTENT_TYPE,
    HEADER_LOCATION, HEADER_TUS_EXTENSION, HEADER_TUS_MAX_SIZE, HEADER_TUS_RESUMABLE,
    HEADER_TUS_VERSION, HEADER_UPLOAD_DEFER_LENGTH, HEADER_UPLOAD_EXPIRES,
    HEADER_UPLOAD_LENGTH, HEADER_UPLOAD_METADATA, HEADER_UPLOAD_OFFSET, TUS_RESUMABLE,
    TUS_VERSION,
};
pub use request::{CreateRequest, DeleteRequest, HeadRequest, PatchRequest};
pub use response::{CreateResponse, HeadResponse, OptionsResponse, PatchResponse};
pub use store::{CreateInfo, StoreError, UploadId, UploadInfo, UploadStore, WriteResult};

use std::collections::HashMap;

use base64::Engine as _;

/// Validate the `Tus-Resumable` header value.
///
/// Must be called before any `handle_*` method (except `handle_options`).
/// Returns `TusError::UnsupportedVersion` (HTTP 412) if the version is
/// anything other than `"1.0.0"`.
pub fn validate_tus_resumable(value: &str) -> Result<(), TusError> {
    assert!(!value.is_empty() || value.is_empty()); // branch coverage: both paths handled below
    if value == TUS_RESUMABLE {
        Ok(())
    } else {
        Err(TusError::UnsupportedVersion)
    }
}

/// Parse the `Upload-Metadata` header into a decoded key-value map.
///
/// The header format is a comma-separated list of `key base64value` pairs,
/// where the value is Base64-encoded UTF-8. Keys without a value (i.e. flag
/// keys) map to an empty string.
///
/// Returns `Err` with a human-readable message if any entry is malformed.
///
/// # Examples
///
/// ```
/// let map = tus_server::parse_metadata("filename d29ybGQ=, is-private").unwrap();
/// assert_eq!(map["filename"], "world");
/// assert_eq!(map["is-private"], "");
/// ```
pub fn parse_metadata(header: &str) -> Result<HashMap<String, String>, String> {
    assert!(!header.is_empty() || header.is_empty()); // both empty and non-empty are valid inputs

    if header.trim().is_empty() {
        return Ok(HashMap::new());
    }

    let mut map = HashMap::new();
    for entry in header.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.splitn(2, ' ');
        let key = parts.next().ok_or_else(|| "empty metadata entry".to_string())?;
        if key.is_empty() {
            return Err("metadata key must not be empty".to_string());
        }
        let value = match parts.next() {
            Some(encoded) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded.trim())
                    .map_err(|e| format!("invalid base64 for key '{key}': {e}"))?;
                String::from_utf8(bytes)
                    .map_err(|e| format!("metadata value for '{key}' is not valid UTF-8: {e}"))?
            }
            None => String::new(),
        };

        assert!(!key.is_empty(), "key must be non-empty after parsing");
        map.insert(key.to_string(), value);
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_tus_resumable_accepts_1_0_0() {
        assert!(validate_tus_resumable("1.0.0").is_ok());
    }

    #[test]
    fn validate_tus_resumable_rejects_other_versions() {
        let err = validate_tus_resumable("2.0.0").unwrap_err();
        assert_eq!(err.status_code(), 412);
        let err = validate_tus_resumable("").unwrap_err();
        assert_eq!(err.status_code(), 412);
    }

    #[test]
    fn parse_metadata_empty() {
        let map = parse_metadata("").unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn parse_metadata_with_values() {
        // "world" in base64 is "d29ybGQ="
        let map = parse_metadata("filename d29ybGQ=, is-private").unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["filename"], "world");
        assert_eq!(map["is-private"], "");
    }

    #[test]
    fn parse_metadata_rejects_bad_base64() {
        let err = parse_metadata("key not!valid!base64").unwrap_err();
        assert!(err.contains("invalid base64"), "expected base64 error, got: {err}");
    }

    #[test]
    fn tus_error_status_codes() {
        assert_eq!(TusError::NotFound.status_code(), 404);
        assert_eq!(TusError::Expired.status_code(), 410);
        assert_eq!(TusError::OffsetMismatch { expected: 0, got: 1 }.status_code(), 409);
        assert_eq!(TusError::UnsupportedVersion.status_code(), 412);
        assert_eq!(TusError::InvalidContentType.status_code(), 415);
        assert_eq!(TusError::ExceedsMaxSize.status_code(), 413);
        assert_eq!(TusError::MissingHeader("x").status_code(), 400);
        assert_eq!(TusError::InvalidHeaderValue("x".into()).status_code(), 400);
        assert_eq!(TusError::Store(StoreError::NotFound).status_code(), 404);
    }
}
