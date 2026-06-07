use std::time::SystemTime;

use crate::{
    protocol::*,
    store::UploadId,
};

fn format_http_date(t: SystemTime) -> String {
    httpdate::fmt_http_date(t)
}

/// Response to an OPTIONS request.
///
/// The caller should return HTTP 204 with these headers.
#[derive(Debug)]
pub struct OptionsResponse {
    /// The TUS version the server implements.
    pub tus_version: &'static str,
    /// Extensions supported by this server.
    pub tus_extensions: &'static [&'static str],
    /// Maximum upload size allowed, or `None` for no limit.
    pub max_size: Option<u64>,
}

impl OptionsResponse {
    pub fn status_code(&self) -> u16 {
        204
    }

    /// HTTP headers to include in the OPTIONS response.
    ///
    /// `Tus-Resumable` is intentionally omitted from OPTIONS per the spec.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![
            (HEADER_TUS_VERSION, self.tus_version.to_string()),
            (
                HEADER_TUS_EXTENSION,
                self.tus_extensions.join(","),
            ),
        ];
        if let Some(max) = self.max_size {
            h.push((HEADER_TUS_MAX_SIZE, max.to_string()));
        }
        h
    }
}

/// Response to a POST (creation) request.
///
/// The caller should return HTTP 201 with a `Location` header pointing to the
/// new upload resource. The `upload_id` is provided so the caller can
/// construct the URL using whatever path prefix their router uses.
#[derive(Debug)]
pub struct CreateResponse {
    /// Identifier of the newly created upload.
    pub upload_id: UploadId,
    /// Expiration time, if the server has an expiration policy.
    pub upload_expires: Option<SystemTime>,
}

impl CreateResponse {
    pub fn status_code(&self) -> u16 {
        201
    }

    /// HTTP headers to include (excluding `Location`, which the caller builds
    /// using `upload_id` and their routing prefix).
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![
            (HEADER_TUS_RESUMABLE, TUS_RESUMABLE.to_string()),
            (HEADER_UPLOAD_OFFSET, "0".to_string()),
        ];
        if let Some(expires) = self.upload_expires {
            h.push((HEADER_UPLOAD_EXPIRES, format_http_date(expires)));
        }
        h
    }
}

/// Response to a HEAD request.
///
/// The caller should return HTTP 200 with these headers and no body.
#[derive(Debug)]
pub struct HeadResponse {
    pub upload_offset: u64,
    /// `None` when the client declared deferred length.
    pub upload_length: Option<u64>,
    pub upload_expires: Option<SystemTime>,
}

impl HeadResponse {
    pub fn status_code(&self) -> u16 {
        200
    }

    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![
            (HEADER_TUS_RESUMABLE, TUS_RESUMABLE.to_string()),
            (HEADER_UPLOAD_OFFSET, self.upload_offset.to_string()),
            (HEADER_CACHE_CONTROL, "no-store".to_string()),
        ];
        if let Some(len) = self.upload_length {
            h.push((HEADER_UPLOAD_LENGTH, len.to_string()));
        }
        if let Some(expires) = self.upload_expires {
            h.push((HEADER_UPLOAD_EXPIRES, format_http_date(expires)));
        }
        h
    }
}

/// Response to a successful PATCH request.
///
/// The caller should return HTTP 204 with these headers and no body.
#[derive(Debug)]
pub struct PatchResponse {
    /// Updated offset after appending the chunk.
    pub new_offset: u64,
    pub upload_expires: Option<SystemTime>,
    /// `true` when `new_offset` equals the declared `Upload-Length`,
    /// meaning all bytes have been received.
    pub upload_complete: bool,
}

impl PatchResponse {
    pub fn status_code(&self) -> u16 {
        204
    }

    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![
            (HEADER_TUS_RESUMABLE, TUS_RESUMABLE.to_string()),
            (HEADER_UPLOAD_OFFSET, self.new_offset.to_string()),
            (HEADER_CACHE_CONTROL, "no-store".to_string()),
        ];
        if let Some(expires) = self.upload_expires {
            h.push((HEADER_UPLOAD_EXPIRES, format_http_date(expires)));
        }
        h
    }
}
