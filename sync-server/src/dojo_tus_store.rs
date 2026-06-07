//! Filesystem-backed [`tus_server::UploadStore`] scoped to one dojo directory.
//!
//! Each upload lives under `{dojo_root}/tus_work/{upload_id}/` with `meta.json`
//! and `data` (append-only payload). The upload offset is always the length of
//! `data` so it cannot drift from `meta.json`.
//!
//! Completed uploads with metadata key [`JJ_REL_PATH_METADATA_KEY`] are finalized into
//! `{dojo_root}/mirror/<jj_rel_path>` by [`FsDojoUploadStore::finalize_into_mirror`].

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tus_server::{CreateInfo, StoreError, UploadId, UploadInfo, UploadStore, WriteResult};

use dojjo_mirror::git_mirror_exclude::jj_repo_rel_excluded_from_git_file_mirror;
use dojjo_mirror::mirror_exclude::jj_repo_rel_excluded_from_mirror;
use dojjo_mirror::mirror_path::parse_safe_mirror_rel;

const TUS_WORK: &str = "tus_work";
pub(crate) const MIRROR_DIR: &str = "mirror";
const META_FILE: &str = "meta.json";
const DATA_FILE: &str = "data";

/// TUS metadata key: UTF-8 relative path under per-dojo `mirror/` (forward slashes).
pub const JJ_REL_PATH_METADATA_KEY: &str = "jj_rel_path";

#[derive(Debug, Error)]
pub enum MirrorFinalizeError {
    #[error("git object store paths must not be mirrored as files; use git push/fetch")]
    GitStorePathRejected,
    #[error("workspace_store paths are machine-local and must not be mirrored")]
    WorkspaceStorePathRejected,
    #[error("missing jj_rel_path in upload metadata")]
    MissingJjRelPath,
    #[error("invalid jj_rel_path: {0}")]
    InvalidJjRelPath(&'static str),
    #[error("completed upload size does not match declared length")]
    LengthMismatch,
    #[error("upload has no declared length")]
    MissingDeclaredLength,
    #[error("upload not found")]
    UploadNotFound,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedMeta {
    id: String,
    length: Option<u64>,
    metadata: HashMap<String, String>,
    expires_at_unix_secs: Option<u64>,
}

impl PersistedMeta {
    fn expires_at(&self) -> Option<SystemTime> {
        let secs = self.expires_at_unix_secs?;
        Some(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

/// TUS upload storage rooted at a single dojo's data directory (`{data_dir}/{dojo_id}/`).
#[derive(Debug)]
pub struct FsDojoUploadStore {
    dojo_root: PathBuf,
    /// Serializes all filesystem mutations for this dojo so concurrent PATCHes cannot corrupt offsets.
    io_lock: Mutex<()>,
}

impl FsDojoUploadStore {
    pub fn new(dojo_root: PathBuf) -> Self {
        assert!(
            !dojo_root.as_os_str().is_empty(),
            "dojo_root must not be empty"
        );
        FsDojoUploadStore {
            dojo_root,
            io_lock: Mutex::new(()),
        }
    }

    /// JJ-normalize `mirror/` after a client finished `dev sync` (same lock as TUS finalize).
    pub async fn normalize_mirror_after_sync(
        &self,
    ) -> Result<(), crate::mirror_normalize::MirrorNormalizeError> {
        let _guard = self.io_lock.lock().await;
        let dojo_root = self.dojo_root.clone();
        tokio::task::spawn_blocking(move || crate::mirror_normalize::normalize_mirror_repo(&dojo_root))
            .await?
    }

    fn tus_work_dir(&self) -> PathBuf {
        self.dojo_root.join(TUS_WORK)
    }

    fn upload_dir(&self, id: &UploadId) -> PathBuf {
        assert!(!id.as_str().is_empty(), "upload id must not be empty");
        self.tus_work_dir().join(id.as_str())
    }

    fn meta_path(upload_dir: &Path) -> PathBuf {
        assert!(
            upload_dir.as_os_str().len() > 0,
            "upload_dir must not be empty"
        );
        upload_dir.join(META_FILE)
    }

    fn data_path(upload_dir: &Path) -> PathBuf {
        assert!(
            upload_dir.as_os_str().len() > 0,
            "upload_dir must not be empty"
        );
        upload_dir.join(DATA_FILE)
    }

    async fn read_meta(upload_dir: &Path) -> Result<PersistedMeta, StoreError> {
        let path = Self::meta_path(upload_dir);
        let bytes = fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound
            } else {
                StoreError::Other(e.into())
            }
        })?;
        assert!(
            !bytes.is_empty(),
            "meta file must not be empty for a valid upload"
        );
        let meta: PersistedMeta = serde_json::from_slice(&bytes).map_err(|e| {
            StoreError::Other(format!("corrupt tus meta at {}: {e}", path.display()).into())
        })?;
        assert!(!meta.id.is_empty(), "persisted meta id must not be empty");
        Ok(meta)
    }

    async fn data_len(upload_dir: &Path) -> Result<u64, StoreError> {
        let path = Self::data_path(upload_dir);
        let meta = fs::metadata(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound
            } else {
                StoreError::Other(e.into())
            }
        })?;
        let len = meta.len();
        assert_eq!(meta.len(), len, "len() must match itself");
        Ok(len)
    }

    /// Move a **fully received** upload's `data` file into `mirror/<jj_rel_path>` and remove the
    /// tus staging directory. Requires metadata [`JJ_REL_PATH_METADATA_KEY`] set at creation time.
    pub async fn finalize_into_mirror(&self, id: &UploadId) -> Result<(), MirrorFinalizeError> {
        let _guard = self.io_lock.lock().await;
        assert!(!id.as_str().is_empty(), "finalize id must not be empty");

        let upload_dir = self.upload_dir(id);
        let meta = match Self::read_meta(&upload_dir).await {
            Ok(m) => m,
            Err(StoreError::NotFound) => return Err(MirrorFinalizeError::UploadNotFound),
            Err(StoreError::Other(e)) => {
                return Err(MirrorFinalizeError::Io(std::io::Error::new(
                    ErrorKind::Other,
                    e.to_string(),
                )));
            }
        };
        assert_eq!(meta.id, id.as_str(), "meta id must match finalize target");

        let rel_raw = meta
            .metadata
            .get(JJ_REL_PATH_METADATA_KEY)
            .ok_or(MirrorFinalizeError::MissingJjRelPath)?;
        assert!(
            !rel_raw.is_empty(),
            "jj_rel_path metadata must not be empty when present"
        );

        let rel_path =
            parse_safe_mirror_rel(rel_raw).map_err(MirrorFinalizeError::InvalidJjRelPath)?;

        let rel_slash = rel_path
            .iter()
            .map(|c| {
                c.to_str()
                    .expect("parse_safe_mirror_rel only yields UTF-8 segments")
            })
            .collect::<Vec<_>>()
            .join("/");
        assert!(!rel_slash.is_empty(), "non-empty rel_path must stringify");
        if jj_repo_rel_excluded_from_mirror(&rel_slash) {
            return Err(MirrorFinalizeError::WorkspaceStorePathRejected);
        }
        if jj_repo_rel_excluded_from_git_file_mirror(&rel_slash) {
            return Err(MirrorFinalizeError::GitStorePathRejected);
        }

        let mirror_root = self.dojo_root.join(MIRROR_DIR);
        let dest = mirror_root.join(&rel_path);
        assert!(
            dest.starts_with(&mirror_root),
            "resolved destination must stay under mirror root"
        );

        let data_path = Self::data_path(&upload_dir);
        let actual_len = Self::data_len(&upload_dir).await.map_err(|e| match e {
            StoreError::NotFound => MirrorFinalizeError::UploadNotFound,
            StoreError::Other(o) => {
                MirrorFinalizeError::Io(std::io::Error::new(ErrorKind::Other, o.to_string()))
            }
        })?;

        let expected = meta
            .length
            .ok_or(MirrorFinalizeError::MissingDeclaredLength)?;
        if actual_len != expected {
            return Err(MirrorFinalizeError::LengthMismatch);
        }
        assert_eq!(actual_len, expected, "length check must be consistent");

        if let Some(parent) = dest.parent() {
            assert!(
                !parent.as_os_str().is_empty(),
                "dest parent must not be empty"
            );
            fs::create_dir_all(parent)
                .await
                .map_err(MirrorFinalizeError::Io)?;
        }

        fs::rename(&data_path, &dest)
            .await
            .map_err(MirrorFinalizeError::Io)?;

        let meta_path = Self::meta_path(&upload_dir);
        fs::remove_file(&meta_path)
            .await
            .map_err(MirrorFinalizeError::Io)?;
        fs::remove_dir(&upload_dir)
            .await
            .map_err(MirrorFinalizeError::Io)?;
        assert!(
            !upload_dir.exists(),
            "staging dir must be removed after finalize"
        );

        let on_disk = fs::metadata(&dest)
            .await
            .map_err(MirrorFinalizeError::Io)?
            .len();
        assert_eq!(
            on_disk, expected,
            "mirrored file size must match declared upload length"
        );

        Ok(())
    }
}

#[async_trait]
impl UploadStore for FsDojoUploadStore {
    async fn create_upload(&self, info: CreateInfo) -> Result<UploadInfo, StoreError> {
        let _guard = self.io_lock.lock().await;

        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        assert!(!id.is_empty(), "generated upload id must not be empty");

        let upload_dir = self.upload_dir(&UploadId::from(id.clone()));
        fs::create_dir_all(&self.tus_work_dir())
            .await
            .map_err(|e| StoreError::Other(e.into()))?;
        fs::create_dir(&upload_dir)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;

        let expires_at_unix_secs = info.expires_at.map(|t| {
            t.duration_since(UNIX_EPOCH)
                .expect("expiration must not be before unix epoch")
                .as_secs()
        });

        let persisted = PersistedMeta {
            id: id.clone(),
            length: info.length,
            metadata: info.metadata.clone(),
            expires_at_unix_secs,
        };

        let meta_json = serde_json::to_vec(&persisted).map_err(|e| StoreError::Other(e.into()))?;
        assert!(!meta_json.is_empty(), "serialized meta must not be empty");

        fs::write(Self::meta_path(&upload_dir), &meta_json)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;
        fs::write(Self::data_path(&upload_dir), [])
            .await
            .map_err(|e| StoreError::Other(e.into()))?;

        let offset = Self::data_len(&upload_dir).await?;
        assert_eq!(offset, 0, "new upload must start at offset 0");

        Ok(UploadInfo {
            id: UploadId::from(id),
            offset: 0,
            length: info.length,
            metadata: info.metadata,
            expires_at: info.expires_at,
        })
    }

    async fn get_upload(&self, id: &UploadId) -> Result<UploadInfo, StoreError> {
        let _guard = self.io_lock.lock().await;
        assert!(!id.as_str().is_empty(), "get_upload id must not be empty");

        let upload_dir = self.upload_dir(id);
        let meta = Self::read_meta(&upload_dir).await?;
        assert_eq!(
            meta.id,
            id.as_str(),
            "meta id must match requested upload id"
        );

        let offset = Self::data_len(&upload_dir).await?;
        assert!(
            meta.length.map_or(true, |len| offset <= len),
            "data on disk must not exceed declared length"
        );

        let expires_at = meta.expires_at();
        let length = meta.length;
        let metadata = meta.metadata;
        Ok(UploadInfo {
            id: id.clone(),
            offset,
            length,
            metadata,
            expires_at,
        })
    }

    async fn write_chunk(
        &self,
        id: &UploadId,
        offset: u64,
        data: Bytes,
    ) -> Result<WriteResult, StoreError> {
        let _guard = self.io_lock.lock().await;
        assert!(!id.as_str().is_empty(), "write_chunk id must not be empty");

        let upload_dir = self.upload_dir(id);
        let meta = Self::read_meta(&upload_dir).await?;
        assert_eq!(meta.id, id.as_str(), "meta id must match upload id");

        let current = Self::data_len(&upload_dir).await?;
        assert_eq!(
            current, offset,
            "store pre-condition: data file length must equal PATCH offset"
        );

        if let Some(len) = meta.length {
            let end = offset
                .checked_add(data.len() as u64)
                .expect("offset + chunk must not overflow");
            assert!(end <= len, "chunk must not exceed declared upload length");
        }

        let path = Self::data_path(&upload_dir);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;
        file.write_all(&data)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;
        file.flush()
            .await
            .map_err(|e| StoreError::Other(e.into()))?;
        drop(file);

        let new_offset = Self::data_len(&upload_dir).await?;
        assert!(new_offset >= offset, "offset must not shrink after append");
        assert_eq!(
            new_offset,
            offset + data.len() as u64,
            "offset must advance by exact chunk size"
        );

        Ok(WriteResult { new_offset })
    }

    async fn delete_upload(&self, id: &UploadId) -> Result<(), StoreError> {
        let _guard = self.io_lock.lock().await;
        assert!(
            !id.as_str().is_empty(),
            "delete_upload id must not be empty"
        );

        let upload_dir = self.upload_dir(id);
        if !upload_dir.exists() {
            return Err(StoreError::NotFound);
        }

        fs::remove_dir_all(&upload_dir)
            .await
            .map_err(|e| StoreError::Other(e.into()))?;
        assert!(
            !upload_dir.exists(),
            "upload directory must be removed after delete"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn create_head_patch_roundtrip() {
        let dir = tempdir().unwrap();
        let store = FsDojoUploadStore::new(dir.path().to_path_buf());

        let info = store
            .create_upload(CreateInfo {
                length: Some(5),
                metadata: HashMap::from([("k".into(), "v".into())]),
                expires_at: None,
            })
            .await
            .unwrap();
        assert_eq!(info.offset, 0);
        assert_eq!(info.length, Some(5));

        let got = store.get_upload(&info.id).await.unwrap();
        assert_eq!(got.offset, 0);

        let r = store
            .write_chunk(&info.id, 0, Bytes::from_static(b"hello"))
            .await
            .unwrap();
        assert_eq!(r.new_offset, 5);

        let got = store.get_upload(&info.id).await.unwrap();
        assert_eq!(got.offset, 5);

        store.delete_upload(&info.id).await.unwrap();
        assert!(store.get_upload(&info.id).await.is_err());
    }

    #[tokio::test]
    async fn finalize_moves_data_to_mirror() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = FsDojoUploadStore::new(root.clone());

        let info = store
            .create_upload(CreateInfo {
                length: Some(3),
                metadata: HashMap::from([(
                    JJ_REL_PATH_METADATA_KEY.to_string(),
                    "a/b.bin".to_string(),
                )]),
                expires_at: None,
            })
            .await
            .unwrap();

        store
            .write_chunk(&info.id, 0, Bytes::from_static(b"abc"))
            .await
            .unwrap();
        store.finalize_into_mirror(&info.id).await.unwrap();

        let dest = root.join("mirror").join("a").join("b.bin");
        let got = std::fs::read(&dest).unwrap();
        assert_eq!(got, b"abc");
        let staging = root.join(TUS_WORK).join(info.id.as_str());
        assert!(!staging.exists(), "tus staging dir must be removed");
    }

    #[tokio::test]
    async fn finalize_rejects_git_object_store_path() {
        let dir = tempdir().unwrap();
        let store = FsDojoUploadStore::new(dir.path().to_path_buf());
        let info = store
            .create_upload(CreateInfo {
                length: Some(1),
                metadata: HashMap::from([(
                    JJ_REL_PATH_METADATA_KEY.to_string(),
                    "store/git/objects/ab/deadbeef".to_string(),
                )]),
                expires_at: None,
            })
            .await
            .unwrap();
        store
            .write_chunk(&info.id, 0, Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = store.finalize_into_mirror(&info.id).await.unwrap_err();
        assert!(matches!(err, MirrorFinalizeError::GitStorePathRejected));
    }

    #[tokio::test]
    async fn finalize_rejects_git_refs_path() {
        let dir = tempdir().unwrap();
        let store = FsDojoUploadStore::new(dir.path().to_path_buf());
        let info = store
            .create_upload(CreateInfo {
                length: Some(1),
                metadata: HashMap::from([(
                    JJ_REL_PATH_METADATA_KEY.to_string(),
                    "store/git/refs/heads/main".to_string(),
                )]),
                expires_at: None,
            })
            .await
            .unwrap();
        store
            .write_chunk(&info.id, 0, Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = store.finalize_into_mirror(&info.id).await.unwrap_err();
        assert!(matches!(err, MirrorFinalizeError::GitStorePathRejected));
    }

    #[tokio::test]
    async fn finalize_rejects_jj_lock_file_path() {
        let dir = tempdir().unwrap();
        let store = FsDojoUploadStore::new(dir.path().to_path_buf());
        let info = store
            .create_upload(CreateInfo {
                length: Some(1),
                metadata: HashMap::from([(
                    JJ_REL_PATH_METADATA_KEY.to_string(),
                    "store/extra/lock".to_string(),
                )]),
                expires_at: None,
            })
            .await
            .unwrap();
        store
            .write_chunk(&info.id, 0, Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = store.finalize_into_mirror(&info.id).await.unwrap_err();
        assert!(matches!(
            err,
            MirrorFinalizeError::WorkspaceStorePathRejected
        ));
    }

    #[tokio::test]
    async fn finalize_rejects_workspace_store_path() {
        let dir = tempdir().unwrap();
        let store = FsDojoUploadStore::new(dir.path().to_path_buf());
        let info = store
            .create_upload(CreateInfo {
                length: Some(1),
                metadata: HashMap::from([(
                    JJ_REL_PATH_METADATA_KEY.to_string(),
                    "workspace_store/workspaces".to_string(),
                )]),
                expires_at: None,
            })
            .await
            .unwrap();
        store
            .write_chunk(&info.id, 0, Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = store.finalize_into_mirror(&info.id).await.unwrap_err();
        assert!(matches!(
            err,
            MirrorFinalizeError::WorkspaceStorePathRejected
        ));
    }

    #[tokio::test]
    async fn finalize_requires_jj_rel_path_metadata() {
        let dir = tempdir().unwrap();
        let store = FsDojoUploadStore::new(dir.path().to_path_buf());
        let info = store
            .create_upload(CreateInfo {
                length: Some(1),
                metadata: HashMap::new(),
                expires_at: None,
            })
            .await
            .unwrap();
        store
            .write_chunk(&info.id, 0, Bytes::from_static(b"x"))
            .await
            .unwrap();
        let err = store.finalize_into_mirror(&info.id).await.unwrap_err();
        assert!(matches!(err, MirrorFinalizeError::MissingJjRelPath));
    }
}
