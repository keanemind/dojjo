//! Relative paths under per-dojo `mirror/` — must reject traversal and odd encodings.

use std::path::{Component, PathBuf};

/// Maximum relative path length we accept (bytes), including separators.
pub const MAX_JJ_REL_PATH_BYTES: usize = 4096;

/// `true` if `seg` is a single non-empty path segment allowed under `mirror/` (no `/`, no `..`).
pub fn is_safe_mirror_path_segment(seg: &str) -> bool {
    if seg.is_empty() {
        return false;
    }
    if seg == "." || seg == ".." {
        return false;
    }
    seg.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Parse and validate a client-supplied path for placement under `mirror/`.
///
/// Rules: trim; no leading `/` or `\\`; no empty or `.` / `..` segments; each segment is
/// non-empty ASCII `[A-Za-z0-9._-]+`; total UTF-8 length at most [`MAX_JJ_REL_PATH_BYTES`].
pub fn parse_safe_mirror_rel(rel: &str) -> Result<PathBuf, &'static str> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("jj_rel_path is empty");
    }
    if rel.len() > MAX_JJ_REL_PATH_BYTES {
        return Err("jj_rel_path exceeds max length");
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err("jj_rel_path must be relative");
    }
    if rel.contains('\\') {
        return Err("jj_rel_path must use forward slashes only");
    }

    let mut out = PathBuf::new();
    for seg in rel.split('/') {
        if seg.is_empty() {
            return Err("jj_rel_path has empty segment");
        }
        if seg == "." || seg == ".." {
            return Err("jj_rel_path must not contain . or .. segments");
        }
        if !is_safe_mirror_path_segment(seg) {
            return Err("jj_rel_path has invalid segment characters");
        }
        out.push(seg);
    }

    assert!(!out.as_os_str().is_empty(), "non-empty rel must yield non-empty path");
    for c in out.components() {
        assert!(
            matches!(c, Component::Normal(_)),
            "path must contain only normal components"
        );
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_typical_jj_subpaths() {
        let p = parse_safe_mirror_rel("op_store/operations/0123456789abcdef").unwrap();
        assert_eq!(
            p,
            PathBuf::from("op_store").join("operations").join("0123456789abcdef")
        );
    }

    #[test]
    fn rejects_parent_segments() {
        assert!(parse_safe_mirror_rel("op_store/../etc/passwd").is_err());
    }

    #[test]
    fn rejects_absolute() {
        assert!(parse_safe_mirror_rel("/op_store/x").is_err());
    }

    #[test]
    fn rejects_bad_chars() {
        assert!(parse_safe_mirror_rel("op_store/foo bar").is_err());
    }
}
