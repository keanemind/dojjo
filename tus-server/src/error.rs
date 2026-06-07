use crate::store::StoreError;

/// All errors that a [`TusHandler`] method can return.
///
/// Protocol errors (wrong version, offset mismatch, etc.) carry enough
/// information for the caller to produce a well-formed HTTP error response.
/// Storage errors are wrapped and surfaced as HTTP 500.
#[derive(Debug, thiserror::Error)]
pub enum TusError {
    /// The upload resource does not exist (HTTP 404).
    #[error("upload not found")]
    NotFound,

    /// The upload has expired and is no longer available (HTTP 410).
    #[error("upload has expired")]
    Expired,

    /// The `Upload-Offset` in the PATCH request does not match the server's
    /// stored offset (HTTP 409).
    #[error("upload offset mismatch: expected {expected}, got {got}")]
    OffsetMismatch { expected: u64, got: u64 },

    /// The client sent a `Tus-Resumable` version the server does not support
    /// (HTTP 412).
    #[error("unsupported tus version")]
    UnsupportedVersion,

    /// The PATCH request body had the wrong `Content-Type` (HTTP 415).
    #[error("Content-Type must be application/offset+octet-stream")]
    InvalidContentType,

    /// The upload size exceeds the server's configured maximum (HTTP 413).
    #[error("upload size exceeds server maximum")]
    ExceedsMaxSize,

    /// A required TUS header was absent (HTTP 400).
    #[error("missing required header: {0}")]
    MissingHeader(&'static str),

    /// A header was present but its value was invalid (HTTP 400).
    #[error("invalid header value: {0}")]
    InvalidHeaderValue(String),

    /// An unexpected storage failure (HTTP 500).
    #[error("storage error: {0}")]
    Store(#[from] StoreError),
}

impl TusError {
    /// The HTTP status code that corresponds to this error.
    pub fn status_code(&self) -> u16 {
        match self {
            TusError::NotFound => 404,
            TusError::Expired => 410,
            TusError::OffsetMismatch { .. } => 409,
            TusError::UnsupportedVersion => 412,
            TusError::InvalidContentType => 415,
            TusError::ExceedsMaxSize => 413,
            TusError::MissingHeader(_) | TusError::InvalidHeaderValue(_) => 400,
            TusError::Store(StoreError::NotFound) => 404,
            TusError::Store(_) => 500,
        }
    }
}
