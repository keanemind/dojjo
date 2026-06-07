//! Update jj's local workspace store (`repo/workspace_store/index`).
//!
//! Join bootstrap attaches the pulled `default` workspace to `_default` without going through
//! `jj workspace add`, so the repo view knows the name but the workspace store has no path.
//! Register the path the same way `SimpleWorkspaceStore::add` would.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use prost::Message;

#[derive(Clone, PartialEq, Message)]
struct WorkspaceEntry {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(bytes = "vec", tag = "2")]
    path: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct Workspaces {
    #[prost(message, repeated, tag = "1")]
    workspaces: Vec<WorkspaceEntry>,
}

struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    fn acquire(store_dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(store_dir)
            .with_context(|| format!("create_dir_all {}", store_dir.display()))?;
        let path = store_dir.join("index.lock");
        for attempt in 0..50 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if attempt == 49 {
                        anyhow::bail!("timed out waiting for workspace store lock {}", path.display());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e).with_context(|| format!("lock {}", path.display())),
            }
        }
        unreachable!("lock retry loop must return");
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn repo_relative_path(repo_path: &Path, workspace_root: &Path) -> anyhow::Result<PathBuf> {
    assert!(repo_path.is_dir(), "repo_path must be a directory");
    assert!(workspace_root.is_dir(), "workspace_root must be a directory");
    let repo_path = repo_path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", repo_path.display()))?;
    let workspace_root = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", workspace_root.display()))?;
    let rel = pathdiff::diff_paths(&workspace_root, &repo_path).with_context(|| {
        format!(
            "relative path from {} to {}",
            repo_path.display(),
            workspace_root.display()
        )
    })?;
    assert!(!rel.as_os_str().is_empty(), "relative workspace path must not be empty");
    Ok(rel)
}

fn path_to_store_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    let s = path
        .to_str()
        .with_context(|| format!("workspace path must be UTF-8: {}", path.display()))?;
    assert!(!s.is_empty(), "workspace path must not be empty");
    Ok(s.replace('\\', "/").into_bytes())
}

fn read_store(store_file: &Path) -> anyhow::Result<Workspaces> {
    if !store_file.is_file() {
        return Ok(Workspaces::default());
    }
    let data = fs::read(store_file).with_context(|| format!("read {}", store_file.display()))?;
    Workspaces::decode(data.as_slice()).with_context(|| format!("decode {}", store_file.display()))
}

fn write_store_atomic(store_file: &Path, workspaces: &Workspaces) -> anyhow::Result<()> {
    let store_dir = store_file
        .parent()
        .context("workspace store file must have a parent directory")?;
    let encoded = workspaces.encode_to_vec();
    assert!(
        !encoded.is_empty() || workspaces.workspaces.is_empty(),
        "encoded store must be valid"
    );
    let tmp = store_dir.join(format!("index.tmp.{}", std::process::id()));
    fs::write(&tmp, &encoded).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, store_file).with_context(|| {
        format!("rename {} -> {}", tmp.display(), store_file.display())
    })?;
    assert!(store_file.is_file(), "workspace store file must exist after write");
    Ok(())
}

/// Record `workspace_root` for `workspace_name` in jj's workspace store at `sync_repo`.
pub fn register_workspace_path(
    sync_repo: &Path,
    workspace_name: &str,
    workspace_root: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(!workspace_name.is_empty(), "workspace_name must not be empty");
    assert!(sync_repo.is_dir(), "sync_repo must be a directory");
    assert!(workspace_root.is_dir(), "workspace_root must be a directory");

    let store_dir = sync_repo.join("workspace_store");
    let store_file = store_dir.join("index");
    let rel = repo_relative_path(sync_repo, workspace_root)?;
    let path_bytes = path_to_store_bytes(&rel)?;

    let _lock = StoreLock::acquire(&store_dir)?;
    let mut workspaces = read_store(&store_file)?;
    workspaces
        .workspaces
        .retain(|entry| entry.name != workspace_name);
    workspaces.workspaces.push(WorkspaceEntry {
        name: workspace_name.to_string(),
        path: path_bytes,
    });
    write_store_atomic(&store_file, &workspaces)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn register_workspace_path_roundtrip() {
        let dir = tempdir().unwrap();
        let default_ws = dir.path().join("_default");
        let sync_jj = default_ws.join(".jj");
        let sync_repo = sync_jj.join("repo");
        fs::create_dir_all(sync_repo.join("store")).unwrap();

        register_workspace_path(&sync_repo, "default", &default_ws).unwrap();

        let got = read_store(&sync_repo.join("workspace_store").join("index")).unwrap();
        assert_eq!(got.workspaces.len(), 1);
        assert_eq!(got.workspaces[0].name, "default");
        assert_eq!(got.workspaces[0].path, b"../..");
    }

    #[test]
    fn register_workspace_path_replaces_existing_name() {
        let dir = tempdir().unwrap();
        let default_ws = dir.path().join("_default");
        let sync_repo = default_ws.join(".jj").join("repo");
        fs::create_dir_all(sync_repo.join("store")).unwrap();

        register_workspace_path(&sync_repo, "default", &default_ws).unwrap();
        register_workspace_path(&sync_repo, "default", &default_ws).unwrap();

        let got = read_store(&sync_repo.join("workspace_store").join("index")).unwrap();
        assert_eq!(got.workspaces.len(), 1);
    }
}
