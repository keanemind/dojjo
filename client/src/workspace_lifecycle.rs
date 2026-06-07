//! Re-home the physical `.jj/repo` under `dojo_home/_default`, mint frozen `default`, user pointer workspace.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::{
    self, default_workspace_root, find_jj_dir_from, write_repo_pointer, DEFAULT_WORKSPACE_NAME,
};
use crate::jj_exec;

/// Local-only bootstrap workspace name (not for user edits); used when `_default` lacks a working copy.
pub const SYNC_HOST_BOOTSTRAP_NAME: &str = "_dojjo_sync_host";

pub fn has_working_copy(workspace_root: &Path) -> bool {
    workspace_root.join(".jj").join("working_copy").exists()
}

/// Ensure `dest` exists and is empty (or only contains `.jj` before we replace it).
fn ensure_empty_workspace_root(dest: &Path) -> anyhow::Result<()> {
    if !dest.exists() {
        fs::create_dir_all(dest).with_context(|| format!("create_dir_all {}", dest.display()))?;
        return Ok(());
    }
    let mut entries = fs::read_dir(dest).with_context(|| format!("read_dir {}", dest.display()))?;
    let first = entries.next();
    if first.is_none() {
        return Ok(());
    }
    let first = first.unwrap()?;
    if entries.next().is_some() {
        anyhow::bail!(
            "workspace destination {} must be empty",
            dest.display()
        );
    }
    let name = first.file_name();
    if name != ".jj" {
        anyhow::bail!(
            "workspace destination {} must be empty (found {:?})",
            dest.display(),
            name
        );
    }
    Ok(())
}

/// Move the physical repo directory to `sync_jj/repo` and write a pointer at `user_jj/repo`.
fn relocate_repo_to_sync_host(
    user_jj: &Path,
    _user_workspace: &Path,
    sync_jj: &Path,
) -> anyhow::Result<PathBuf> {
    let user_repo_entry = user_jj.join("repo");
    assert!(
        user_repo_entry.is_dir(),
        "user workspace must host the physical repo directory before relocation"
    );

    fs::create_dir_all(sync_jj).with_context(|| format!("create_dir_all {}", sync_jj.display()))?;
    let dest_repo = sync_jj.join("repo");

    if dest_repo.exists() {
        if dest_repo.is_file() {
            fs::remove_file(&dest_repo)
                .with_context(|| format!("remove pointer {}", dest_repo.display()))?;
        } else {
            fs::remove_dir_all(&dest_repo)
                .with_context(|| format!("remove {}", dest_repo.display()))?;
        }
    }

    fs::rename(&user_repo_entry, &dest_repo).with_context(|| {
        format!(
            "move repo {} -> {}",
            user_repo_entry.display(),
            dest_repo.display()
        )
    })?;

    anyhow::ensure!(
        !user_jj.join("repo").exists(),
        "user .jj/repo must be gone after move (still present at {})",
        user_jj.join("repo").display()
    );

    let sync_repo = dest_repo
        .canonicalize()
        .with_context(|| format!("canonicalize {}", dest_repo.display()))?;
    write_repo_pointer(user_jj, &sync_repo)?;
    Ok(sync_repo)
}

/// Move the physical repo from `sync_jj/repo` back to `user_jj/repo` (inverse of [`relocate_repo_to_sync_host`]).
pub fn unrehome_repo_to_user(user_jj: &Path, sync_jj: &Path) -> anyhow::Result<PathBuf> {
    assert!(user_jj.as_os_str().len() > 0, "user_jj must not be empty");
    assert!(sync_jj.as_os_str().len() > 0, "sync_jj must not be empty");

    let user_repo_entry = user_jj.join("repo");
    assert!(
        user_repo_entry.is_file(),
        "user .jj/repo must be a pointer file before unrehome (got {})",
        user_repo_entry.display()
    );

    let sync_repo = sync_jj.join("repo");
    assert!(
        sync_repo.is_dir(),
        "sync .jj/repo must be a directory before unrehome (got {})",
        sync_repo.display()
    );

    fs::remove_file(&user_repo_entry)
        .with_context(|| format!("remove repo pointer {}", user_repo_entry.display()))?;
    anyhow::ensure!(
        !user_repo_entry.exists(),
        "user .jj/repo pointer must be gone before move"
    );

    fs::rename(&sync_repo, &user_repo_entry).with_context(|| {
        format!(
            "move repo {} -> {}",
            sync_repo.display(),
            user_repo_entry.display()
        )
    })?;

    let user_repo = user_repo_entry
        .canonicalize()
        .with_context(|| format!("canonicalize {}", user_repo_entry.display()))?;
    assert!(user_repo.is_dir(), "user .jj/repo must be a directory after unrehome");
    assert!(
        !sync_repo.exists(),
        "sync .jj/repo must be gone after move (still at {})",
        sync_repo.display()
    );
    Ok(user_repo)
}

const GIT_BACKEND_TYPE: &str = "git";
const GIT_TARGET_INTERNAL: &str = "git";

/// True when the repo uses the Git backend (`store/type` == `git`).
pub fn repo_uses_git_backend(sync_repo: &Path) -> anyhow::Result<bool> {
    let store_type = sync_repo.join("store").join("type");
    if !store_type.is_file() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&store_type)
        .with_context(|| format!("read {}", store_type.display()))?;
    Ok(raw.trim() == GIT_BACKEND_TYPE)
}

/// Write host-local `store/git_target` and ensure Git objects live under `store/git/`.
pub fn write_host_git_target(sync_repo: &Path) -> anyhow::Result<()> {
    assert!(sync_repo.is_dir(), "sync_repo must be a directory");
    let store = sync_repo.join("store");
    assert!(store.is_dir(), "sync repo must have store/");
    let git_target_path = store.join("git_target");
    std::fs::write(&git_target_path, GIT_TARGET_INTERNAL.as_bytes())
        .with_context(|| format!("write {}", git_target_path.display()))?;
    Ok(())
}

fn assert_internal_git_store(sync_repo: &Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(sync_repo.join("store").join("git_target"))
        .context("read store/git_target")?;
    let trimmed = raw.trim();
    anyhow::ensure!(
        trimmed == GIT_TARGET_INTERNAL,
        "dojo sync host requires internal git store (git_target must be {GIT_TARGET_INTERNAL:?}, got {trimmed:?})"
    );
    let git_store = sync_repo.join("store").join("git");
    anyhow::ensure!(
        git_store.is_dir(),
        "git backend sync host must have {}",
        git_store.display()
    );
    Ok(())
}

/// Non-colocated git backend on the sync host: `git_target` = `git`, objects under `store/git/`.
pub fn ensure_sync_host_non_colocated_git(sync_repo: &Path) -> anyhow::Result<()> {
    assert!(sync_repo.is_dir(), "sync_repo must be a directory");
    if !repo_uses_git_backend(sync_repo)? {
        return Ok(());
    }
    write_host_git_target(sync_repo)?;
    let git_store = sync_repo.join("store").join("git");
    crate::git_sync::ensure_git_dir_layout(&git_store)
        .with_context(|| format!("bootstrap {}", git_store.display()))?;
    Ok(())
}

/// Run `jj git colocation disable` while the repo still lives under `jj_workspace` (before re-home).
fn disable_git_colocation_before_rehome(jj_workspace: &Path, repo_at_jj: &Path) -> anyhow::Result<()> {
    assert!(jj_workspace.is_dir(), "jj_workspace must be a directory");
    assert!(repo_at_jj.is_dir(), "repo_at_jj must be a directory");
    if !repo_uses_git_backend(repo_at_jj)? {
        return Ok(());
    }
    jj_exec::git_colocation_disable(jj_workspace).with_context(|| {
        format!(
            "jj git colocation disable before re-home (workspace {})",
            jj_workspace.display()
        )
    })?;
    assert_internal_git_store(repo_at_jj)
}

/// Idempotent: safe to call after an interrupted `dojjo create` (skips completed JJ steps).
pub fn ensure_dojo_after_create(
    user_workspace: &Path,
    dojo_home: &Path,
    user_workspace_name: &str,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        user_workspace_name != DEFAULT_WORKSPACE_NAME,
        "user workspace name must not be {DEFAULT_WORKSPACE_NAME}"
    );

    let user_jj = config::find_jj_dir_from(user_workspace)?;
    let default_ws = default_workspace_root(dojo_home);
    let sync_jj = default_ws.join(".jj");
    fs::create_dir_all(&default_ws)
        .with_context(|| format!("create_dir_all {}", default_ws.display()))?;

    if config::workspace_rehomed_to_dojo(&user_jj, dojo_home)? {
        let sync_repo = config::sync_repo_root(dojo_home)?;
        assert!(sync_repo.is_dir(), "sync repo must exist after re-home");
        ensure_sync_host_non_colocated_git(&sync_repo)?;
        return Ok(sync_repo);
    }

    ensure_empty_workspace_root(&default_ws)?;

    if jj_exec::workspace_exists(user_workspace, DEFAULT_WORKSPACE_NAME)? {
        jj_exec::workspace_rename(user_workspace, user_workspace_name)?;
    }

    if !jj_exec::workspace_exists(user_workspace, DEFAULT_WORKSPACE_NAME)? {
        jj_exec::workspace_add_frozen_default(user_workspace, &default_ws)?;
    }

    let user_repo = user_jj.join("repo");
    assert!(user_repo.is_dir(), "repo must still be local before re-home");
    disable_git_colocation_before_rehome(user_workspace, &user_repo)?;

    let sync_repo = relocate_repo_to_sync_host(&user_jj, user_workspace, &sync_jj)?;
    assert!(
        sync_repo == config::sync_repo_root(dojo_home)?,
        "sync repo path must match dojo home _default host"
    );
    ensure_sync_host_non_colocated_git(&sync_repo)?;
    Ok(sync_repo)
}

/// Return `_default` workspace root, creating a loadable working copy via `jj -R` when needed.
pub fn ensure_sync_host_workspace(
    dojo_home: &Path,
    anchor_workspace: &Path,
) -> anyhow::Result<PathBuf> {
    let default_ws = default_workspace_root(dojo_home);
    fs::create_dir_all(&default_ws)
        .with_context(|| format!("create_dir_all {}", default_ws.display()))?;

    if has_working_copy(&default_ws) {
        ensure_sentinel_default_workspace_registered(dojo_home)?;
        ensure_local_workspace_registered(dojo_home, anchor_workspace)?;
        return Ok(default_ws);
    }

    assert!(
        has_working_copy(anchor_workspace),
        "anchor workspace must have a working copy"
    );
    jj_exec::workspace_add_with_repository(
        anchor_workspace,
        &default_ws,
        SYNC_HOST_BOOTSTRAP_NAME,
    )?;
    ensure_sentinel_default_workspace_registered(dojo_home)?;
    ensure_local_workspace_registered(dojo_home, anchor_workspace)?;
    Ok(default_ws)
}

/// Add a new user workspace on a cold machine (no pre-existing JJ checkout on this host).
pub fn cold_join_user_workspace(
    dojo_home: &Path,
    into: &Path,
    workspace_name: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        workspace_name != DEFAULT_WORKSPACE_NAME,
        "workspace name must not be {DEFAULT_WORKSPACE_NAME}"
    );
    anyhow::ensure!(
        workspace_name != SYNC_HOST_BOOTSTRAP_NAME,
        "workspace name must not be {SYNC_HOST_BOOTSTRAP_NAME}"
    );

    let sync_host = bootstrap_sync_host_after_pull(dojo_home)?;
    assert!(
        sync_host.as_os_str().len() > 0,
        "sync host path must be non-empty"
    );

    if jj_exec::workspace_exists_at_repository(&sync_host, workspace_name)? {
        anyhow::bail!(
            "workspace name {workspace_name:?} already exists in this dojo; pick another --name"
        );
    }

    ensure_empty_workspace_root(into)?;
    jj_exec::workspace_add_with_repository(&sync_host, into, workspace_name)?;

    // `jj workspace add` may write an absolute repo pointer (macOS /var vs /private/var).
    // Normalize to a relative pointer when possible so dojjo and jj agree on disk layout.
    let user_jj = find_jj_dir_from(into)?;
    let sync_repo = config::sync_repo_root(dojo_home)?;
    write_repo_pointer(&user_jj, &sync_repo)
        .with_context(|| format!("normalize repo pointer for workspace {}", into.display()))?;
    Ok(())
}

/// After mirror pull, make `_default` loadable by `jj` and return its workspace root.
pub fn bootstrap_sync_host_after_pull(dojo_home: &Path) -> anyhow::Result<PathBuf> {
    let default_ws = default_workspace_root(dojo_home);
    fs::create_dir_all(&default_ws)
        .with_context(|| format!("create_dir_all {}", default_ws.display()))?;

    let sync_jj = default_ws.join(".jj");
    fs::create_dir_all(&sync_jj).with_context(|| format!("create_dir_all {}", sync_jj.display()))?;

    let sync_repo = config::sync_repo_root(dojo_home)?;
    assert!(sync_repo.is_dir(), "sync repo must exist after pull");

    if !has_working_copy(&default_ws) {
        seed_sync_host_working_copy(&sync_jj, DEFAULT_WORKSPACE_NAME)?;
    }

    ensure_sentinel_default_workspace_registered(dojo_home)?;

    assert!(
        has_working_copy(&default_ws),
        "sync host must have working_copy after bootstrap"
    );
    Ok(default_ws)
}

fn ensure_sentinel_default_workspace_registered(dojo_home: &Path) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let default_ws = default_workspace_root(dojo_home);
    if !has_working_copy(&default_ws) {
        return Ok(());
    }
    let sync_repo = config::sync_repo_root(dojo_home)?;
    crate::jj_workspace_store::register_workspace_path(
        &sync_repo,
        DEFAULT_WORKSPACE_NAME,
        &default_ws,
    )
    .with_context(|| {
        format!(
            "register jj workspace store path for {DEFAULT_WORKSPACE_NAME} at {}",
            default_ws.display()
        )
    })
}

/// Register the anchor workspace path in jj's workspace store (per-machine metadata).
fn ensure_local_workspace_registered(
    dojo_home: &Path,
    anchor_workspace: &Path,
) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let anchor_workspace = anchor_workspace
        .canonicalize()
        .with_context(|| format!("canonicalize {}", anchor_workspace.display()))?;
    let jj_dir = config::find_jj_dir_from(&anchor_workspace)?;
    if !config::workspace_rehomed_to_dojo(&jj_dir, dojo_home)? {
        return Ok(());
    }
    let name = jj_exec::current_workspace_name(&anchor_workspace)?;
    if name == DEFAULT_WORKSPACE_NAME {
        return Ok(());
    }
    if name == SYNC_HOST_BOOTSTRAP_NAME {
        return Ok(());
    }
    let sync_repo = config::sync_repo_root(dojo_home)?;
    crate::jj_workspace_store::register_workspace_path(&sync_repo, &name, &anchor_workspace)
        .with_context(|| {
            format!(
                "register jj workspace store path for {name} at {}",
                anchor_workspace.display()
            )
        })
}

/// Minimal `working_copy/` so `jj` can open the pulled repo at the sync host (no anchor workspace).
fn seed_sync_host_working_copy(sync_jj: &Path, workspace_name: &str) -> anyhow::Result<()> {
    assert!(sync_jj.as_os_str().len() > 0, "sync_jj must not be empty");
    anyhow::ensure!(!workspace_name.is_empty(), "workspace_name must not be empty");

    let sync_repo = config::resolve_repo_path_at_jj(sync_jj)?;
    let op_id = read_repo_head_operation_id(&sync_repo)?;
    assert!(!op_id.is_empty(), "head operation id must not be empty");

    let wc = sync_jj.join("working_copy");
    fs::create_dir_all(&wc).with_context(|| format!("create_dir_all {}", wc.display()))?;

    let type_path = wc.join("type");
    fs::write(&type_path, "local").with_context(|| format!("write {}", type_path.display()))?;

    let checkout_bytes = encode_checkout_proto(&op_id, workspace_name);
    assert!(!checkout_bytes.is_empty(), "checkout proto must not be empty");
    let checkout_path = wc.join("checkout");
    fs::write(&checkout_path, &checkout_bytes)
        .with_context(|| format!("write {}", checkout_path.display()))?;
    Ok(())
}

fn read_repo_head_operation_id(sync_repo: &Path) -> anyhow::Result<Vec<u8>> {
    let heads_dir = sync_repo.join("op_heads").join("heads");
    anyhow::ensure!(
        heads_dir.is_dir(),
        "repo missing op_heads/heads at {}",
        heads_dir.display()
    );

    let mut names: Vec<String> = Vec::new();
    for ent in fs::read_dir(&heads_dir).with_context(|| format!("read_dir {}", heads_dir.display()))? {
        let ent = ent?;
        if ent.file_type().context("file_type")?.is_file() {
            let name = ent.file_name();
            let name = name
                .to_str()
                .with_context(|| format!("non-utf8 op head file name {:?}", name))?;
            assert!(!name.is_empty(), "op head file name must not be empty");
            names.push(name.to_string());
        }
    }
    anyhow::ensure!(!names.is_empty(), "repo has no operation heads (empty dojo?)");
    names.sort();
    let hex_id = &names[0];
    let op_id = hex::decode(hex_id).with_context(|| format!("decode op head hex {hex_id:?}"))?;
    assert!(!op_id.is_empty(), "decoded op head must not be empty");
    Ok(op_id)
}

fn write_varint(mut n: u64, out: &mut Vec<u8>) {
    assert!(out.len() < out.capacity() + 16, "varint buffer must have space");
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn push_len_delimited_field(field_number: u32, payload: &[u8], out: &mut Vec<u8>) {
    assert!(field_number > 0, "field_number must be positive");
    assert!(field_number < 190, "field_number must fit in one-byte tag");
    let tag = (field_number << 3) | 2;
    write_varint(tag as u64, out);
    write_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// Encode `local_working_copy.Checkout` (operation_id + workspace_name).
fn encode_checkout_proto(operation_id: &[u8], workspace_name: &str) -> Vec<u8> {
    assert!(!operation_id.is_empty(), "operation_id must not be empty");
    assert!(!workspace_name.is_empty(), "workspace_name must not be empty");

    let mut out = Vec::new();
    push_len_delimited_field(2, operation_id, &mut out);
    push_len_delimited_field(3, workspace_name.as_bytes(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn unrehome_roundtrip_with_relocate() {
        let dir = tempdir().unwrap();
        let user_ws = dir.path().join("user");
        let sync_ws = dir.path().join("sync");
        let user_jj = user_ws.join(".jj");
        let sync_jj = sync_ws.join(".jj");
        let host_repo = user_jj.join("repo");
        fs::create_dir_all(host_repo.join("store")).unwrap();

        relocate_repo_to_sync_host(&user_jj, &user_ws, &sync_jj).unwrap();
        assert!(user_jj.join("repo").is_file());
        assert!(sync_jj.join("repo").is_dir());

        let back = unrehome_repo_to_user(&user_jj, &sync_jj).unwrap();
        assert_eq!(back, host_repo.canonicalize().unwrap());
        assert!(user_jj.join("repo").is_dir());
        assert!(!sync_jj.join("repo").exists());
    }

    #[test]
    fn checkout_proto_matches_jj_layout() {
        let op_id = vec![0xab; 64];
        let got = encode_checkout_proto(&op_id, "default");
        assert_eq!(got.len(), 2 + 64 + 2 + 7);
        assert_eq!(got[0], 0x12);
        assert_eq!(got[1], 64);
        assert_eq!(&got[2..66], op_id.as_slice());
        assert_eq!(&got[66..], &[0x1a, 0x07, b'd', b'e', b'f', b'a', b'u', b'l', b't']);
    }
}
