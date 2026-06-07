/// The single TUS protocol version this library implements.
pub const TUS_RESUMABLE: &str = "1.0.0";
pub const TUS_VERSION: &str = "1.0.0";

// Standard header names in lowercase (HTTP/2 canonical form).
pub const HEADER_TUS_RESUMABLE: &str = "tus-resumable";
pub const HEADER_TUS_VERSION: &str = "tus-version";
pub const HEADER_TUS_EXTENSION: &str = "tus-extension";
pub const HEADER_TUS_MAX_SIZE: &str = "tus-max-size";
pub const HEADER_UPLOAD_OFFSET: &str = "upload-offset";
pub const HEADER_UPLOAD_LENGTH: &str = "upload-length";
pub const HEADER_UPLOAD_DEFER_LENGTH: &str = "upload-defer-length";
pub const HEADER_UPLOAD_METADATA: &str = "upload-metadata";
pub const HEADER_UPLOAD_EXPIRES: &str = "upload-expires";
pub const HEADER_CACHE_CONTROL: &str = "cache-control";
pub const HEADER_LOCATION: &str = "location";
pub const HEADER_CONTENT_TYPE: &str = "content-type";

pub const CONTENT_TYPE_OFFSET_OCTET_STREAM: &str = "application/offset+octet-stream";

// Extensions supported by this library.
pub const EXTENSIONS: &[&str] = &["creation", "termination", "expiration"];

const _: () = {
    assert!(!TUS_RESUMABLE.is_empty(), "TUS_RESUMABLE must be non-empty");
    assert!(!TUS_VERSION.is_empty(), "TUS_VERSION must be non-empty");
    assert!(!EXTENSIONS.is_empty(), "EXTENSIONS must list at least one extension");
};
