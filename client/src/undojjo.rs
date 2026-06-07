//! `dojjo undojjo` — remove local dojjo linkage and restore a standalone JJ repo at `cwd`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::{
    self, default_workspace_root, path_is_under, repoint_sibling_workspaces,
    workspace_root_from_jj, DEFAULT_WORKSPACE_NAME, DOJJO_LINK_FILE,
};
use crate::jj_exec;
use crate::jj_workspace_store;
use crate::workspace_lifecycle;

pub async fn run(cwd: &Path) -> anyhow::Result<()> {
    assert!(cwd.as_os_str().len() > 0, "cwd must not be empty");

    let user_workspace = cwd
        .canonicalize()
        .with_context(|| format!("canonicalize {}", cwd.display()))?;
    let user_jj = config::find_jj_dir_from(&user_workspace).context(
        "dojjo undojjo must run inside a Jujutsu workspace (no .jj/repo found)",
    )?;

    let dojo_home = resolve_dojo_home(&user_workspace, &user_jj)?;
    let dojo_id = resolve_dojo_id(&dojo_home, &user_jj)?;

    if !config::workspace_rehomed_to_dojo(&user_jj, &dojo_home)? {
        cleanup_metadata_only(&dojo_home, &user_jj)?;
        println!(
            "removed incomplete dojjo setup for {dojo_id} (repo was not re-homed); server dojo unchanged"
        );
        return Ok(());
    }

    run_full_undojjo(&user_workspace, &user_jj, &dojo_home, &dojo_id).await
}

fn resolve_dojo_home(user_workspace: &Path, user_jj: &Path) -> anyhow::Result<PathBuf> {
    if let Some(link) = config::read_workspace_link(user_jj)? {
        return config::dojo_home(&link.dojo_id);
    }
    config::find_dojo_home_for_workspace(user_workspace)
}

fn resolve_dojo_id(dojo_home: &Path, user_jj: &Path) -> anyhow::Result<String> {
    if let Some(link) = config::read_workspace_link(user_jj)? {
        return Ok(link.dojo_id);
    }
    if config::dojo_config_path(dojo_home).is_file() {
        let cfg = config::load_config(dojo_home)?;
        return Ok(cfg.dojo_id);
    }
    anyhow::bail!(
        "no dojo id for {}; missing .jj/{DOJJO_LINK_FILE}",
        dojo_home.display()
    );
}

fn cleanup_metadata_only(dojo_home: &Path, user_jj: &Path) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let link = user_jj.join(DOJJO_LINK_FILE);
    if link.is_file() {
        fs::remove_file(&link).with_context(|| format!("remove {}", link.display()))?;
    }
    if dojo_home.exists() {
        fs::remove_dir_all(dojo_home)
            .with_context(|| format!("remove {}", dojo_home.display()))?;
    }
    Ok(())
}

async fn run_full_undojjo(
    user_workspace: &Path,
    user_jj: &Path,
    dojo_home: &Path,
    dojo_id: &str,
) -> anyhow::Result<()> {
    assert!(!dojo_id.is_empty(), "dojo_id must not be empty");

    let default_host = default_workspace_root(dojo_home);
    let default_host = default_host
        .canonicalize()
        .with_context(|| format!("canonicalize {}", default_host.display()))?;
    let dojjo_base = config::dojjo_home_base()?
        .canonicalize()
        .context("canonicalize dojjo home base")?;

    let user_workspace = user_workspace
        .canonicalize()
        .with_context(|| format!("canonicalize {}", user_workspace.display()))?;

    anyhow::ensure!(
        user_workspace != default_host,
        "dojjo undojjo must not run from the sentinel _default workspace ({})",
        default_host.display()
    );

    let current_name = jj_exec::current_workspace_name(&user_workspace)?;
    anyhow::ensure!(
        current_name != DEFAULT_WORKSPACE_NAME,
        "dojjo undojjo must run from your project workspace, not the reserved name {DEFAULT_WORKSPACE_NAME}"
    );

    anyhow::ensure!(
        jj_exec::workspace_exists(&user_workspace, DEFAULT_WORKSPACE_NAME)?,
        "workspace {DEFAULT_WORKSPACE_NAME} not found; repo is not in post-create/join layout"
    );

    let sentinel_root = match jj_exec::try_workspace_root_for_name(&user_workspace, DEFAULT_WORKSPACE_NAME)?
    {
        Some(root) => root,
        None => {
            // Repair stale local workspace-store entry for `default` after repo-base changes.
            let sync_repo = config::resolve_repo_path_at_jj(user_jj)?;
            jj_workspace_store::register_workspace_path(
                &sync_repo,
                DEFAULT_WORKSPACE_NAME,
                &default_host,
            )
            .context("repair workspace_store path for default")?;
            jj_exec::workspace_root_for_name(&user_workspace, DEFAULT_WORKSPACE_NAME)?
        }
    };
    anyhow::ensure!(
        path_is_under(&sentinel_root, &default_host),
        "workspace {DEFAULT_WORKSPACE_NAME} must be rooted at {} (got {})",
        default_host.display(),
        sentinel_root.display()
    );
    anyhow::ensure!(
        path_is_under(&sentinel_root, &dojjo_base),
        "workspace {DEFAULT_WORKSPACE_NAME} must be under {} (got {})",
        dojjo_base.display(),
        sentinel_root.display()
    );

    let entries = jj_exec::list_workspaces(&user_workspace)?;
    let mut workspace_roots: Vec<(String, PathBuf)> = Vec::new();
    for entry in &entries {
        let Some(root) = jj_exec::try_workspace_root_for_name(&user_workspace, &entry.name)
            .with_context(|| format!("resolve workspace root for {}", entry.name))?
        else {
            continue;
        };
        assert!(root.is_dir(), "workspace root must be a directory");
        workspace_roots.push((entry.name.clone(), root));
    }
    anyhow::ensure!(
        workspace_roots.iter().any(|(name, root)| {
            name == &current_name && root == &user_workspace
        }),
        "current workspace {current_name} must have a recorded path on this machine"
    );

    for (name, root) in &workspace_roots {
        if root.starts_with(&default_host) {
            continue;
        }
        let jj = root.join(".jj");
        anyhow::ensure!(
            jj.is_dir(),
            "workspace {name} at {} must have .jj/ for repoint",
            root.display()
        );
        anyhow::ensure!(
            jj.join("repo").exists(),
            "workspace {name} at {} must have .jj/repo",
            root.display()
        );
    }

    jj_exec::workspace_forget(&user_workspace, DEFAULT_WORKSPACE_NAME)
        .context("jj workspace forget default")?;

    jj_exec::workspace_rename(&user_workspace, DEFAULT_WORKSPACE_NAME)
        .context("jj workspace rename to default")?;

    let sync_jj = default_host.join(".jj");
    let new_repo = workspace_lifecycle::unrehome_repo_to_user(user_jj, &sync_jj)
        .context("move physical repo back to user workspace")?;
    assert!(
        new_repo == config::resolve_repo_path_at_jj(user_jj)?,
        "repo after unrehome must match resolve_repo_path_at_jj"
    );
    rewrite_local_workspace_store_after_unrehome(
        &workspace_roots,
        &current_name,
        &user_workspace,
        &default_host,
        &new_repo,
    )?;

    repoint_sibling_workspaces(
        &workspace_roots,
        &user_workspace,
        &default_host,
        &new_repo,
    )
    .context("repoint sibling workspace repo pointers")?;

    if workspace_lifecycle::repo_uses_git_backend(&new_repo)? {
        let git_dir = crate::git_sync::resolve_jj_backed_git_dir(&new_repo)?;
        crate::git_sync::remove_dojjo_remote(&git_dir)
            .await
            .context("git remote remove dojjo")?;
    }

    if dojo_home.exists() {
        fs::remove_dir_all(dojo_home)
            .with_context(|| format!("remove {}", dojo_home.display()))?;
    }

    let link = user_jj.join(DOJJO_LINK_FILE);
    if link.is_file() {
        fs::remove_file(&link).with_context(|| format!("remove {}", link.display()))?;
    }

    let ws_root = workspace_root_from_jj(user_jj)?;
    assert!(
        ws_root == user_workspace,
        "user workspace root must match cwd"
    );

    println!("undojjo complete for {dojo_id}; standalone repo at {}", new_repo.display());
    println!("server dojo unchanged");
    Ok(())
}

fn rewrite_local_workspace_store_after_unrehome(
    workspace_roots: &[(String, PathBuf)],
    current_name_before_rename: &str,
    user_workspace: &Path,
    default_host: &Path,
    new_repo: &Path,
) -> anyhow::Result<()> {
    assert!(!current_name_before_rename.is_empty(), "current workspace name must not be empty");
    assert!(user_workspace.is_dir(), "user workspace must be a directory");
    assert!(default_host.is_dir(), "default host must be a directory");
    assert!(new_repo.is_dir(), "new_repo must be a directory");

    for (name, root) in workspace_roots {
        if root.starts_with(default_host) {
            continue;
        }
        let stored_name = if name == current_name_before_rename {
            DEFAULT_WORKSPACE_NAME
        } else {
            name.as_str()
        };
        let stored_root = if name == current_name_before_rename {
            user_workspace
        } else {
            root.as_path()
        };
        if !stored_root.is_dir() {
            continue;
        }
        jj_workspace_store::register_workspace_path(new_repo, stored_name, stored_root)
            .with_context(|| format!("register workspace_store path for {stored_name}"))?;
    }
    Ok(())
}
