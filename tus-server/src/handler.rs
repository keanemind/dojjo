use std::time::{Duration, SystemTime};

use crate::{
    error::TusError,
    protocol::*,
    request::{CreateRequest, DeleteRequest, HeadRequest, PatchRequest},
    response::{CreateResponse, HeadResponse, OptionsResponse, PatchResponse},
    store::{CreateInfo, UploadStore, WriteResult},
};

/// Configuration for a [`TusHandler`].
#[derive(Debug, Clone)]
pub struct TusConfig {
    /// Maximum upload size in bytes. `None` means unlimited.
    pub max_size: Option<u64>,
    /// How long newly created uploads survive before expiring.
    /// `None` means uploads do not expire.
    pub upload_expiration: Option<Duration>,
}

impl TusConfig {
    pub fn new() -> Self {
        TusConfig {
            max_size: None,
            upload_expiration: None,
        }
    }
}

impl Default for TusConfig {
    fn default() -> Self {
        TusConfig::new()
    }
}

/// Framework-agnostic TUS 1.0.0 protocol handler.
///
/// The caller is responsible for:
///
/// 1. **Routing** – directing HTTP requests to the appropriate `handle_*`
///    method based on method and path.
/// 2. **Header extraction** – pulling `Tus-Resumable`, `Upload-Offset`, etc.
///    from the raw request and constructing the typed request structs.
/// 3. **Version validation** – calling [`validate_tus_resumable`] before
///    invoking any handler method except `handle_options`.
/// 4. **Response mapping** – converting the typed response structs to whatever
///    HTTP response type the framework expects, using the `status_code()` and
///    `headers()` methods on each response.
///
/// [`validate_tus_resumable`]: crate::validate_tus_resumable
pub struct TusHandler<S> {
    store: S,
    config: TusConfig,
}

impl<S: UploadStore> TusHandler<S> {
    pub fn new(store: S, config: TusConfig) -> Self {
        // Compile-time sanity checks on protocol constants.
        assert!(!TUS_RESUMABLE.is_empty(), "TUS_RESUMABLE constant must be non-empty");
        assert!(!EXTENSIONS.is_empty(), "EXTENSIONS list must be non-empty");
        // Runtime validation of the config.
        assert!(
            config.max_size.map_or(true, |s| s > 0),
            "max_size must be positive when set"
        );
        TusHandler { store, config }
    }

    /// Handle an OPTIONS request.
    ///
    /// Per the spec the `Tus-Resumable` header is NOT required in OPTIONS
    /// requests or responses. No version validation is performed.
    pub fn handle_options(&self) -> OptionsResponse {
        OptionsResponse {
            tus_version: TUS_VERSION,
            tus_extensions: EXTENSIONS,
            max_size: self.config.max_size,
        }
    }

    /// Handle a POST request to create a new upload (creation extension).
    ///
    /// The caller must have already validated `Tus-Resumable` via
    /// [`validate_tus_resumable`].
    ///
    /// Returns a [`CreateResponse`] containing the new `upload_id`. The caller
    /// must construct the `Location` header from this id and their URL prefix.
    ///
    /// [`validate_tus_resumable`]: crate::validate_tus_resumable
    pub async fn handle_create(&self, req: CreateRequest) -> Result<CreateResponse, TusError> {
        // Pre-condition: one of upload_length or defer_length must be set.
        if req.upload_length.is_none() && !req.defer_length {
            return Err(TusError::MissingHeader("upload-length"));
        }
        // Pre-condition: upload_length and upload-defer-length are mutually exclusive.
        if req.upload_length.is_some() && req.defer_length {
            return Err(TusError::InvalidHeaderValue(
                "upload-length and upload-defer-length are mutually exclusive".into(),
            ));
        }
        if let Some(len) = req.upload_length {
            if let Some(max) = self.config.max_size {
                if len > max {
                    return Err(TusError::ExceedsMaxSize);
                }
            }
        }

        let expires_at = self.config.upload_expiration.map(|d| SystemTime::now() + d);

        let info = self
            .store
            .create_upload(CreateInfo {
                length: req.upload_length,
                metadata: req.metadata,
                expires_at,
            })
            .await?;

        // Post-conditions: assert the store returned a valid, fresh upload.
        assert!(!info.id.0.is_empty(), "store must return a non-empty upload id");
        assert!(
            info.offset == 0,
            "store must initialize a new upload with offset 0, got {}",
            info.offset
        );
        assert!(
            info.length == req.upload_length,
            "store must preserve the declared upload length (expected {:?}, got {:?})",
            req.upload_length,
            info.length
        );

        Ok(CreateResponse {
            upload_id: info.id,
            upload_expires: info.expires_at,
        })
    }

    /// Handle a HEAD request for upload progress.
    ///
    /// Returns `TusError::Expired` (HTTP 410) if the upload exists but has
    /// passed its expiration time.
    pub async fn handle_head(&self, req: HeadRequest) -> Result<HeadResponse, TusError> {
        assert!(!req.upload_id.0.is_empty(), "upload id must not be empty");

        let info = self.store.get_upload(&req.upload_id).await?;

        // Assert store invariants: the returned data must be internally consistent.
        assert!(!info.id.0.is_empty(), "store must return a non-empty upload id");
        assert!(
            info.length.map_or(true, |len| info.offset <= len),
            "store returned offset ({}) exceeding declared length ({:?})",
            info.offset,
            info.length
        );

        if let Some(expires_at) = info.expires_at {
            if expires_at < SystemTime::now() {
                return Err(TusError::Expired);
            }
        }

        Ok(HeadResponse {
            upload_offset: info.offset,
            upload_length: info.length,
            upload_expires: info.expires_at,
        })
    }

    /// Handle a PATCH request to append a chunk to an upload.
    ///
    /// Returns `TusError::OffsetMismatch` (HTTP 409) when the client's
    /// `Upload-Offset` does not match the server's stored offset.
    pub async fn handle_patch(&self, req: PatchRequest) -> Result<PatchResponse, TusError> {
        assert!(!req.upload_id.0.is_empty(), "upload id must not be empty");

        if req.content_type.as_str() != CONTENT_TYPE_OFFSET_OCTET_STREAM {
            return Err(TusError::InvalidContentType);
        }

        let info = self.store.get_upload(&req.upload_id).await?;

        // Assert store invariants on the fetched info.
        assert!(
            info.length.map_or(true, |len| info.offset <= len),
            "store returned offset ({}) exceeding declared length ({:?})",
            info.offset,
            info.length
        );

        if let Some(expires_at) = info.expires_at {
            if expires_at < SystemTime::now() {
                return Err(TusError::Expired);
            }
        }

        if info.offset != req.upload_offset {
            return Err(TusError::OffsetMismatch {
                expected: info.offset,
                got: req.upload_offset,
            });
        }

        if let Some(len) = info.length {
            let end_offset = req
                .upload_offset
                .checked_add(req.data.len() as u64)
                .expect("upload offset + chunk size must not overflow u64");
            if end_offset > len {
                return Err(TusError::InvalidHeaderValue(format!(
                    "chunk end offset {end_offset} exceeds declared upload length {len}"
                )));
            }
        }

        let result: WriteResult = self
            .store
            .write_chunk(&req.upload_id, req.upload_offset, req.data)
            .await?;

        // Post-conditions: assert the store advanced the offset correctly.
        assert!(
            result.new_offset >= req.upload_offset,
            "write_chunk must not decrease the offset (was {}, now {})",
            req.upload_offset,
            result.new_offset
        );
        assert!(
            info.length.map_or(true, |len| result.new_offset <= len),
            "write_chunk advanced offset ({}) past declared length ({:?})",
            result.new_offset,
            info.length
        );

        let upload_complete = match info.length {
            Some(len) => {
                assert!(
                    result.new_offset <= len,
                    "new offset must not exceed declared length"
                );
                result.new_offset == len
            }
            None => false,
        };

        Ok(PatchResponse {
            new_offset: result.new_offset,
            upload_expires: info.expires_at,
            upload_complete,
        })
    }

    /// Handle a DELETE request to remove an upload (termination extension).
    ///
    /// Returns `TusError::NotFound` (HTTP 404) if the upload does not exist.
    pub async fn handle_delete(&self, req: DeleteRequest) -> Result<(), TusError> {
        assert!(!req.upload_id.0.is_empty(), "upload id must not be empty");

        // Fetch first so we surface NotFound with the correct 404, rather than
        // silently succeeding when nothing was there.
        let info = self.store.get_upload(&req.upload_id).await?;
        assert!(
            info.id == req.upload_id,
            "store must return info for the requested upload id"
        );

        self.store.delete_upload(&req.upload_id).await?;

        Ok(())
    }
}
