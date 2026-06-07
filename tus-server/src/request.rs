use std::collections::HashMap;

use bytes::Bytes;

use crate::store::UploadId;

/// Request to create a new upload resource (POST, creation extension).
///
/// At least one of `upload_length` or `defer_length` must be set.
#[derive(Debug)]
pub struct CreateRequest {
    /// Total upload size in bytes. `None` when `defer_length` is `true`.
    pub upload_length: Option<u64>,
    /// Whether the client is deferring the length declaration
    /// (`Upload-Defer-Length: 1`).
    pub defer_length: bool,
    /// Decoded metadata key-value pairs from the `Upload-Metadata` header.
    /// Values are already base64-decoded; use [`crate::parse_metadata`] to
    /// produce this from the raw header string.
    pub metadata: HashMap<String, String>,
}

/// Request to retrieve upload progress (HEAD).
#[derive(Debug)]
pub struct HeadRequest {
    pub upload_id: UploadId,
}

/// Request to append a chunk of data to an upload (PATCH).
#[derive(Debug)]
pub struct PatchRequest {
    pub upload_id: UploadId,
    /// The offset the client believes the upload is at (`Upload-Offset`).
    pub upload_offset: u64,
    /// Value of the `Content-Type` header. Must be
    /// `application/offset+octet-stream`.
    pub content_type: String,
    /// The raw chunk bytes from the request body.
    pub data: Bytes,
}

/// Request to delete an upload resource (DELETE, termination extension).
#[derive(Debug)]
pub struct DeleteRequest {
    pub upload_id: UploadId,
}
