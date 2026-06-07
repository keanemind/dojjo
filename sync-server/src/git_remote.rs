//! Build `git_remote_url` values returned to clients (`file://` locally, HTTP on a VPS).
//!
//! Set [`DOJJO_PUBLIC_URL`] (e.g. `http://example.ts.net:3000`) for smart HTTP remotes at
//! `{public}/git/{dojo_id}.git`. Omit it for local/e2e `file://` paths under [`DOJJO_DATA_DIR`].

use std::path::{Path, PathBuf};

use crate::tus_handlers::assert_safe_dojo_external_id;
use crate::AppError;

/// Client-visible origin for Git (and related) URLs. Set via [`DOJJO_PUBLIC_URL`].
#[derive(Debug, Clone)]
pub struct PublicUrl {
    base: String,
}

impl PublicUrl {
    pub fn parse(value: &str) -> Result<Self, AppError> {
        let base = value.trim().to_string();
        assert!(
            !base.is_empty(),
            "DOJJO_PUBLIC_URL must not be empty when set"
        );
        if base.ends_with('/') {
            return Err(AppError::BadRequest);
        }
        let scheme = if base.starts_with("http://") {
            "http://"
        } else if base.starts_with("https://") {
            "https://"
        } else {
            return Err(AppError::BadRequest);
        };
        let after_scheme = base
            .strip_prefix(scheme)
            .expect("scheme prefix just verified");
        if after_scheme.is_empty() {
            return Err(AppError::BadRequest);
        }
        if after_scheme.contains('/') {
            return Err(AppError::BadRequest);
        }
        assert!(base.contains("://"), "public URL must contain scheme");
        Ok(PublicUrl { base })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Smart HTTP remote for this dojo (`{base}/git/{dojo_id}.git`).
    pub fn git_remote_url(&self, dojo_id: &str) -> Result<String, AppError> {
        assert_safe_dojo_external_id(dojo_id)?;
        let url = format!("{}/git/{}.git", self.base, dojo_id);
        assert!(url.contains("/git/"), "git remote URL must contain /git/");
        assert!(url.ends_with(".git"), "git remote URL must end with .git");
        Ok(url)
    }
}

/// Bare repo directory name under each dojo data directory (`{dojo_root}/bare.git`).
pub fn bare_git_subpath() -> &'static str {
    "bare.git"
}

/// Absolute bare repo path for a dojo under `data_dir`.
pub fn bare_repo_path(data_dir: &Path, dojo_id: &str) -> Result<PathBuf, AppError> {
    assert_safe_dojo_external_id(dojo_id)?;
    assert!(data_dir.is_absolute(), "data_dir must be absolute");
    let bare = data_dir.join(dojo_id).join(bare_git_subpath());
    assert!(
        bare.starts_with(data_dir),
        "bare path must stay under data_dir"
    );
    Ok(bare)
}

/// URL handed to `git remote add dojjo` for this dojo.
pub fn git_remote_url_for_dojo(
    data_dir: &Path,
    dojo_id: &str,
    public_url: Option<&PublicUrl>,
) -> Result<String, AppError> {
    assert_safe_dojo_external_id(dojo_id)?;
    assert!(data_dir.is_absolute(), "data_dir must be absolute");

    if let Some(public) = public_url {
        return public.git_remote_url(dojo_id);
    }

    let bare = bare_repo_path(data_dir, dojo_id)?;
    let canon = bare
        .canonicalize()
        .map_err(|e| AppError::Internal(anyhow::Error::from(e)))?;
    let path = canon.to_str().ok_or(AppError::BadRequest)?;
    assert!(!path.is_empty(), "canonical bare path must not be empty");
    if !path.starts_with('/') {
        return Err(AppError::BadRequest);
    }
    Ok(format!("file://{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn public_url_rejects_trailing_slash() {
        assert!(PublicUrl::parse("http://host:3000/").is_err());
    }

    #[test]
    fn public_url_rejects_path_suffix() {
        assert!(PublicUrl::parse("http://host:3000/api").is_err());
    }

    #[test]
    fn public_url_accepts_host_and_port() {
        let u = PublicUrl::parse("http://example.ts.net:3000").unwrap();
        assert_eq!(u.base(), "http://example.ts.net:3000");
        let url = u.git_remote_url("my-dojo").unwrap();
        assert_eq!(url, "http://example.ts.net:3000/git/my-dojo.git");
    }

    #[test]
    fn file_uri_when_public_url_unset() {
        let dir = tempdir().unwrap();
        let data = dir.path().join("data");
        let bare = data.join("dojo-abc").join("bare.git");
        fs::create_dir_all(&bare).unwrap();
        let data = data.canonicalize().unwrap();
        let url = git_remote_url_for_dojo(&data, "dojo-abc", None).unwrap();
        assert!(url.starts_with("file://"));
        assert!(url.contains("bare.git"));
        assert!(!url.contains("/git/"));
    }

    #[test]
    fn http_url_when_public_url_set() {
        let dir = tempdir().unwrap();
        let data = dir.path().canonicalize().unwrap();
        let public = PublicUrl::parse("http://127.0.0.1:3000").unwrap();
        fs::create_dir_all(data.join("x").join("bare.git")).unwrap();
        let url = git_remote_url_for_dojo(&data, "x", Some(&public)).unwrap();
        assert_eq!(url, "http://127.0.0.1:3000/git/x.git");
    }

    #[test]
    fn rejects_unsafe_dojo_id_in_url() {
        let public = PublicUrl::parse("http://host:3000").unwrap();
        assert!(public.git_remote_url("../x").is_err());
    }
}
