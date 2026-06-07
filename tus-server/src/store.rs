use std::{collections::HashMap, time::SystemTime};

use async_trait::async_trait;
use bytes::Bytes;

/// Opaque identifier for an upload resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UploadId(pub String);

impl UploadId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for UploadId {
    fn from(s: String) -> Self {
        assert!(!s.is_empty(), "UploadId must not be empty");
        UploadId(s)
    }
}

impl std::fmt::Display for UploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Information about an upload resource as stored.
#[derive(Debug, Clone)]
pub struct UploadInfo {
    pub id: UploadId,
    /// Bytes successfully received so far.
    pub offset: u64,
    /// Declared total upload size. None if deferred.
    pub length: Option<u64>,
    /// Key-value metadata supplied at creation time.
    pub metadata: HashMap<String, String>,
    /// When this upload expires. None if no expiration.
    pub expires_at: Option<SystemTime>,
}

/// Parameters for creating a new upload.
#[derive(Debug)]
pub struct CreateInfo {
    /// Total upload size. None if the client deferred declaration.
    pub length: Option<u64>,
    /// Decoded metadata key-value pairs.
    pub metadata: HashMap<String, String>,
    /// Requested expiration time, computed by [`TusHandler`] from the config.
    pub expires_at: Option<SystemTime>,
}

/// Outcome of a successful [`UploadStore::write_chunk`] call.
#[derive(Debug)]
pub struct WriteResult {
    /// The new upload offset after appending the chunk.
    pub new_offset: u64,
}

/// Errors that an [`UploadStore`] implementation may return.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("upload not found")]
    NotFound,
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

/// Persistent storage backend for TUS upload resources.
///
/// Implementations must be `Send + Sync` so that they can be shared across
/// async tasks (e.g. inside an Axum handler behind an `Arc`).
///
/// # Contract
///
/// - [`create_upload`] must return an [`UploadInfo`] with `offset == 0` and
///   `length` matching the `CreateInfo::length` that was passed in.
/// - [`get_upload`] must return `StoreError::NotFound` for unknown ids.
/// - [`write_chunk`] must only advance the offset by exactly `data.len()`
///   bytes and must not exceed the declared `length`.
/// - [`delete_upload`] must return `StoreError::NotFound` if the upload does
///   not exist.
#[async_trait]
pub trait UploadStore: Send + Sync {
    async fn create_upload(&self, info: CreateInfo) -> Result<UploadInfo, StoreError>;
    async fn get_upload(&self, id: &UploadId) -> Result<UploadInfo, StoreError>;
    async fn write_chunk(
        &self,
        id: &UploadId,
        offset: u64,
        data: Bytes,
    ) -> Result<WriteResult, StoreError>;
    async fn delete_upload(&self, id: &UploadId) -> Result<(), StoreError>;
}
