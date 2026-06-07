//! JJ-normalize the per-dojo `mirror/` tree after a client finishes `dev sync`.
//!
//! Clients never mirror `store/git_target` (host-local; see `dojjo_mirror::mirror_exclude`).
//! The sync-server persists its own `mirror/store/git_target` pointing at `{dojo}/bare.git` so
//! stock `RepoLoader::load_at_head()` and store/op_store `gc()` run without custom JJ orchestration.

use std::path::Path;
use std::time::SystemTime;

use jj_lib::config::{ConfigGetError, StackedConfig};
use jj_lib::op_store::OpStoreError;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{RepoLoader, RepoLoaderError, StoreFactories, StoreLoadError};
use jj_lib::settings::UserSettings;
use jj_lib::stacked_table::{TableStore, TableStoreError};
use thiserror::Error;

use crate::dojo_tus_store::MIRROR_DIR;
use crate::git_remote::bare_git_subpath;

const GIT_BACKEND_TYPE: &str = "git";

/// Git-backend extra metadata table key width (see `jj_lib::git_backend::HASH_LENGTH`).
const EXTRA_TABLE_KEY_BYTES: usize = 20;

/// BLAKE2b-512 segment / head marker filename length (see `jj_lib::stacked_table`).
const EXTRA_SEGMENT_FILE_NAME_LEN: usize = 128;

/// Relative path from `mirror/store/` to `{dojo}/bare.git` (sibling of `mirror/`).
const SERVER_MIRROR_GIT_TARGET: &str = "../../bare.git";

#[derive(Debug, Error)]
pub enum MirrorNormalizeError {
    #[error("normalize task join: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("mirror repo not initialized at {mirror_root}")]
    MirrorNotInitialized { mirror_root: String },
    #[error("mirror store backend is not git: {found:?}")]
    NotGitBackend { found: String },
    #[error("bare git repo missing at {path}")]
    BareGitMissing { path: String },
    #[error("server git_target {expected:?} must resolve to bare git (got {resolved:?})")]
    GitTargetLayoutMismatch { expected: String, resolved: String },
    #[error("JJ settings: {0}")]
    Settings(#[from] ConfigGetError),
    #[error("JJ store load: {0}")]
    StoreLoad(#[from] StoreLoadError),
    #[error("JJ load_at_head: {0}")]
    LoadAtHead(#[from] RepoLoaderError),
    #[error("extra table store: {0}")]
    ExtraTable(#[from] TableStoreError),
    #[error("JJ op_store gc: {0}")]
    OpStoreGc(#[from] OpStoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Write persistent server `store/git_target` → `{dojo}/bare.git` (JJ-relative from `store/`).
fn ensure_server_git_target(store_path: &Path, bare_git: &Path) -> Result<(), MirrorNormalizeError> {
    assert!(store_path.is_dir(), "store_path must be a directory");
    assert!(bare_git.as_os_str().len() > 0, "bare_git must not be empty");

    let bare_canon = bare_git.canonicalize().map_err(|_| MirrorNormalizeError::BareGitMissing {
        path: bare_git.display().to_string(),
    })?;
    assert!(bare_canon.is_dir(), "bare git path must be a directory");

    let via_target = store_path.join(SERVER_MIRROR_GIT_TARGET);
    let resolved = via_target.canonicalize().map_err(|_| {
        MirrorNormalizeError::GitTargetLayoutMismatch {
            expected: SERVER_MIRROR_GIT_TARGET.to_string(),
            resolved: via_target.display().to_string(),
        }
    })?;
    assert!(
        resolved == bare_canon,
        "git_target must resolve to bare.git"
    );

    let git_target_path = store_path.join("git_target");
    assert!(
        git_target_path.starts_with(store_path),
        "git_target path must stay under store/"
    );
    std::fs::write(&git_target_path, SERVER_MIRROR_GIT_TARGET.as_bytes())?;
    Ok(())
}

/// JJ `add_head` writes an empty file at `store/extra/heads/<segment-id>`.
///
/// `get_head_locked()` can merge divergent markers yet leave **no** marker files on disk
/// (see `JJ_EXTRA_TABLE_HEADS_BUG.md`). Cold join mirrors files only, so we restore the
/// pointer after JJ picks the canonical segment id.
fn persist_extra_table_head_marker(
    extra_path: &Path,
    segment_id: &str,
) -> Result<(), MirrorNormalizeError> {
    assert!(!segment_id.is_empty(), "segment_id must not be empty");
    assert!(
        segment_id.len() == EXTRA_SEGMENT_FILE_NAME_LEN,
        "segment_id must be a stacked-table segment filename"
    );
    assert!(
        segment_id.chars().all(|c| c.is_ascii_hexdigit()),
        "segment_id must be hex"
    );

    let segment_path = extra_path.join(segment_id);
    assert!(
        segment_path.starts_with(extra_path),
        "segment path must stay under extra/"
    );
    assert!(
        segment_path.is_file(),
        "canonical extra segment file must exist on mirror: {}",
        segment_path.display()
    );

    let heads_dir = extra_path.join("heads");
    assert!(heads_dir.starts_with(extra_path), "heads path must stay under extra/");
    std::fs::create_dir_all(&heads_dir)?;

    let marker_path = heads_dir.join(segment_id);
    assert!(
        marker_path.starts_with(&heads_dir),
        "marker path must stay under heads/"
    );
    if !marker_path.is_file() {
        std::fs::write(&marker_path, b"")?;
    }
    assert!(marker_path.is_file(), "head marker must exist after persist");
    Ok(())
}

/// Merge divergent `store/extra/heads/*` markers on the mirror only (no `git gc` on bare.git).
fn normalize_extra_table_heads(mirror_root: &Path) -> Result<(), MirrorNormalizeError> {
    let extra_path = mirror_root.join("store").join("extra");
    if !extra_path.is_dir() {
        return Ok(());
    }

    let store = TableStore::load(extra_path.clone(), EXTRA_TABLE_KEY_BYTES);
    let table = store.get_head()?;
    let segment_id = table.name();
    assert!(!segment_id.is_empty(), "canonical extra table head must have a name");
    persist_extra_table_head_marker(&extra_path, segment_id)?;
    Ok(())
}

/// Open `dojo_root/mirror/` with JJ and persist publication-pointer cleanup after client sync.
pub fn normalize_mirror_repo(dojo_root: &Path) -> Result<(), MirrorNormalizeError> {
    assert!(dojo_root.as_os_str().len() > 0, "dojo_root must not be empty");
    let mirror_root = dojo_root.join(MIRROR_DIR);
    assert!(
        mirror_root.starts_with(dojo_root),
        "mirror path must stay under dojo root"
    );

    let store_type_path = mirror_root.join("store").join("type");
    if !store_type_path.is_file() {
        return Err(MirrorNormalizeError::MirrorNotInitialized {
            mirror_root: mirror_root.display().to_string(),
        });
    }
    let backend_type = std::fs::read_to_string(&store_type_path)?;
    let backend_type = backend_type.trim();
    assert!(!backend_type.is_empty(), "store/type must not be empty");
    if backend_type != GIT_BACKEND_TYPE {
        return Err(MirrorNormalizeError::NotGitBackend {
            found: backend_type.to_string(),
        });
    }

    let store_path = mirror_root.join("store");
    assert!(store_path.is_dir(), "mirror store/ must exist for git backend");

    let bare_git = dojo_root.join(bare_git_subpath());
    ensure_server_git_target(&store_path, &bare_git)?;

    let config = StackedConfig::with_defaults();
    let settings = UserSettings::from_config(config)?;
    let factories = StoreFactories::default();
    let loader = RepoLoader::init_from_file_system(&settings, &mirror_root, &factories)?;
    // `load_at_head` runs `resolve_op_heads` (ancestor op-head markers removed via `update_op_heads`).
    let repo = loader.load_at_head()?;
    let op_id = repo.op_id().clone();
    assert!(!op_id.hex().is_empty(), "load_at_head must yield non-empty operation id");

    // Mirror-only cleanup. Do not call `repo.store().gc()`: GitBackend `gc` runs `git gc` on
    // `bare.git`, which races with concurrent client push/fetch.
    normalize_extra_table_heads(&mirror_root)?;
    loader
        .op_store()
        .gc(std::slice::from_ref(&op_id), SystemTime::UNIX_EPOCH)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn server_git_target_resolves_to_bare_git() {
        let tmp = tempfile::tempdir().unwrap();
        let dojo = tmp.path();
        let mirror_store = dojo.join("mirror").join("store");
        fs::create_dir_all(&mirror_store).unwrap();
        fs::create_dir_all(dojo.join("bare.git")).unwrap();
        ensure_server_git_target(&mirror_store, &dojo.join("bare.git")).unwrap();
        let raw = fs::read_to_string(mirror_store.join("git_target")).unwrap();
        assert_eq!(raw, SERVER_MIRROR_GIT_TARGET);
    }
}
