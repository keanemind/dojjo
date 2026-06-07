//! Join a dojo: cold pull on first setup on this machine, warm workspace-add when already local.

use std::path::{Path, PathBuf};

use anyhow::Context;
use reqwest::Client;

use crate::background_sync;
use crate::config::{self, DojoConfig, DojoState};
use crate::jj_exec;
use crate::manifest::fetch_manifest;
use crate::mirror_pull::pull_mirror_into_repo;
use crate::tus;
use crate::workspace_lifecycle;

/// Workspace root must be empty (no `.jj` yet).
fn prepare_workspace_root(into: &Path) -> anyhow::Result<PathBuf> {
    if !into.exists() {
        std::fs::create_dir_all(into).with_context(|| format!("create_dir_all {}", into.display()))?;
    }
    let canon = into
        .canonicalize()
        .with_context(|| format!("canonicalize {}", into.display()))?;
    if canon.join(".jj").exists() {
        anyhow::bail!(
            "workspace destination {} already has .jj/; pick an empty directory",
            canon.display()
        );
    }
    for ent in std::fs::read_dir(&canon).with_context(|| format!("read_dir {}", canon.display()))? {
        let ent = ent?;
        anyhow::ensure!(
            ent.file_name() == ".dojjo",
            "directory {} must be empty (found {:?})",
            canon.display(),
            ent.file_name()
        );
    }
    Ok(canon)
}

fn resolve_workspace_name(name: Option<&str>) -> anyhow::Result<String> {
    if let Some(n) = name {
        anyhow::ensure!(!n.is_empty(), "workspace name must not be empty");
        return Ok(n.to_string());
    }
    config::default_user_workspace_name()
}

/// Join must not run from inside an existing JJ workspace (cold start only).
fn assert_cwd_not_jj_workspace(cwd: &Path) -> anyhow::Result<()> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("canonicalize {}", cwd.display()))?;
    if config::find_jj_dir_from(&cwd).is_ok() {
        anyhow::bail!(
            "dojjo join must not run from inside a Jujutsu workspace (cwd {}); run from a neutral directory",
            cwd.display()
        );
    }
    Ok(())
}

/// True when this machine finished a prior create/join: config, sync repo, and loadable sync host.
fn is_local_dojo_join_ready(dojo_home: &Path) -> anyhow::Result<bool> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    if !config::is_dojo_create_complete(dojo_home) {
        return Ok(false);
    }
    let existing = config::load_config(dojo_home)?;
    assert!(!existing.dojo_id.is_empty(), "dojo_id in config must not be empty");
    let _sync_repo = config::sync_repo_root(dojo_home)?;
    let default_ws = config::default_workspace_root(dojo_home);
    Ok(workspace_lifecycle::has_working_copy(&default_ws))
}

fn save_config_if_changed(dojo_home: &Path, cfg: &DojoConfig) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let existing = config::load_config(dojo_home)?;
    if existing == *cfg {
        return Ok(());
    }
    config::save_config(dojo_home, cfg).context("save dojo config")
}

async fn run_cold_join(
    client: &Client,
    dojo_home: &Path,
    dojo_id: &str,
    sync_repo: &Path,
    api_base: &str,
    git_remote_url: &str,
) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    assert!(sync_repo.is_dir(), "sync_repo must be a directory");
    anyhow::ensure!(!dojo_id.is_empty(), "dojo_id must not be empty");
    assert!(!api_base.is_empty(), "api_base must not be empty");
    assert!(!git_remote_url.is_empty(), "git_remote_url must not be empty");

    let pulled = pull_mirror_into_repo(client, api_base, dojo_id, sync_repo).await?;
    if pulled > 0 {
        println!("pulled {pulled} mirror objects into sync repo…");
    }

    let sync_repo = config::sync_repo_root(dojo_home)?;
    workspace_lifecycle::write_host_git_target(&sync_repo).context("write host git_target")?;
    let sync_host = workspace_lifecycle::bootstrap_sync_host_after_pull(dojo_home)?;
    if sync_host.join(".git").is_dir() {
        jj_exec::git_colocation_disable(&sync_host).context("jj git colocation disable on sync host")?;
    }
    workspace_lifecycle::ensure_sync_host_non_colocated_git(&sync_repo)
        .context("non-colocated git store on sync host")?;

    // Git objects are not in the JJ mirror; fetch before any `jj` command touches the repo.
    let git_dir = crate::git_sync::resolve_jj_backed_git_dir(&sync_repo)?;
    crate::git_sync::git_fetch_dojjo(&git_dir, git_remote_url)
        .await
        .context("git fetch from dojjo bare remote (after JJ mirror)")?;

    let cfg = DojoConfig {
        dojo_id: dojo_id.to_string(),
        api_base: api_base.to_string(),
        git_remote_url: Some(git_remote_url.to_string()),
    };
    config::save_config(dojo_home, &cfg).context("save dojo config")?;

    let man = fetch_manifest(client, api_base, dojo_id).await?;
    assert!(
        !man.revision.is_empty(),
        "manifest revision must not be empty"
    );
    config::save_state(
        dojo_home,
        &DojoState {
            last_remote_revision: man.revision,
        },
    )
    .context("save state")?;

    Ok(())
}

fn finish_join(
    dojo_home: &Path,
    dojo_id: &str,
    into: &Path,
    workspace_name: &str,
    warm: bool,
) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    anyhow::ensure!(!dojo_id.is_empty(), "dojo_id must not be empty");
    assert!(into.as_os_str().len() > 0, "into must not be empty");
    anyhow::ensure!(!workspace_name.is_empty(), "workspace_name must not be empty");

    workspace_lifecycle::cold_join_user_workspace(dojo_home, into, workspace_name)?;

    let user_jj = config::find_jj_dir_from(into)?;
    config::write_workspace_link(&user_jj, dojo_id).context("write workspace dojjo link")?;
    background_sync::ensure_worker_running(dojo_home).context("start background sync worker")?;

    let sync_repo = config::sync_repo_root(dojo_home)?;
    if warm {
        println!(
            "warm join: added workspace {workspace_name} at {} (shared sync repo unchanged)",
            into.display()
        );
    } else {
        println!(
            "joined dojo {dojo_id}: workspace {workspace_name} at {}",
            into.display()
        );
    }
    println!("sync repo {}", sync_repo.display());
    println!("config {}", config::dojo_config_path(dojo_home).display());
    println!(
        "{}",
        background_sync::join_or_create_status_message(dojo_home)?
    );
    Ok(())
}

pub async fn run(
    client: &Client,
    dojo_id: &str,
    into: &Path,
    workspace_name: Option<&str>,
    api_base: &str,
    cwd: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let api_base = config::normalize_api_base(api_base);
    assert!(!api_base.is_empty(), "api_base must not be empty");

    assert_cwd_not_jj_workspace(cwd)?;

    let into = prepare_workspace_root(into)?;
    let workspace_name = resolve_workspace_name(workspace_name)?;

    let public = tus::fetch_dojo_public(client, &api_base, dojo_id)
        .await
        .context("fetch dojo metadata (git_remote_url)")?;
    anyhow::ensure!(
        public.id == dojo_id,
        "dojo id mismatch: expected {dojo_id}, got {}",
        public.id
    );

    let dojo_home = config::dojo_home(&dojo_id)?;
    std::fs::create_dir_all(&dojo_home)
        .with_context(|| format!("create_dir_all {}", dojo_home.display()))?;

    if is_local_dojo_join_ready(&dojo_home)? {
        let existing = config::load_config(&dojo_home)?;
        anyhow::ensure!(
            existing.dojo_id == dojo_id,
            "this machine already has dojo {} set up; --dojo-id {} does not match",
            existing.dojo_id,
            dojo_id
        );
        let merged = DojoConfig {
            dojo_id: dojo_id.to_string(),
            api_base: api_base.clone(),
            git_remote_url: Some(public.git_remote_url.clone()),
        };
        save_config_if_changed(&dojo_home, &merged).context("update dojo config")?;
        finish_join(&dojo_home, dojo_id, &into, &workspace_name, true)?;
        return Ok(());
    }

    let sync_jj = config::default_workspace_root(&dojo_home).join(".jj");
    std::fs::create_dir_all(&sync_jj)
        .with_context(|| format!("create_dir_all {}", sync_jj.display()))?;

    let sync_repo = match config::sync_repo_root(&dojo_home) {
        Ok(p) => p,
        Err(_) => {
            let sync_repo = sync_jj.join("repo");
            std::fs::create_dir_all(&sync_repo)
                .with_context(|| format!("create_dir_all {}", sync_repo.display()))?;
            sync_repo
        }
    };
    assert!(sync_repo.is_dir(), "sync repo must be a directory");

    run_cold_join(
        client,
        &dojo_home,
        dojo_id,
        &sync_repo,
        &api_base,
        &public.git_remote_url,
    )
    .await?;

    finish_join(&dojo_home, dojo_id, &into, &workspace_name, false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn is_local_dojo_join_ready_false_without_config() {
        let dir = tempdir().unwrap();
        assert!(!is_local_dojo_join_ready(dir.path()).unwrap());
    }

    #[test]
    fn is_local_dojo_join_ready_true_with_config_sync_repo_and_working_copy() {
        let dir = tempdir().unwrap();
        let dojo_home = dir.path().join("dojos").join("d1");
        let default_ws = config::default_workspace_root(&dojo_home);
        let sync_jj = default_ws.join(".jj");
        let sync_repo = sync_jj.join("repo");
        std::fs::create_dir_all(&sync_repo).unwrap();
        std::fs::create_dir_all(sync_jj.join("working_copy")).unwrap();
        std::fs::write(sync_jj.join("working_copy").join("type"), "local").unwrap();

        let cfg = DojoConfig {
            dojo_id: "d1".to_string(),
            api_base: "http://localhost:3000/api".to_string(),
            git_remote_url: None,
        };
        config::save_config(&dojo_home, &cfg).unwrap();

        assert!(is_local_dojo_join_ready(&dojo_home).unwrap());
    }
}
