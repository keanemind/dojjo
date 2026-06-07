//! Create a dojo: re-home repo under `~/.dojjo/dojos/{id}/_default`, upload, link user workspace.
//!
//! Idempotent: interrupted runs resume from `create_progress.json` / `.jj/dojjo.json`.

use std::path::Path;

use anyhow::Context;
use dojjo_mirror::mirror_apply_order::compute_mirror_apply_order;
use reqwest::Client;
use sha2::{Digest, Sha256};

use crate::config::{
    self, CreatePhase, CreateProgress, DojoConfig, DojoState,
};
use crate::manifest::fetch_manifest;
use crate::repo_walk::scan_repo_files;
use crate::tus::{fetch_dojo_public, post_create_dojo, tus_upload_bytes};
use crate::workspace_lifecycle;
use crate::background_sync;

pub async fn run(client: &Client, cwd: &Path, api_base: &str) -> anyhow::Result<()> {
    assert!(cwd.as_os_str().len() > 0, "cwd must not be empty");
    let api_base = config::normalize_api_base(api_base);
    assert!(
        !api_base.is_empty(),
        "api_base after normalize must not be empty"
    );

    let user_workspace = cwd
        .canonicalize()
        .with_context(|| format!("canonicalize {}", cwd.display()))?;
    let _ = config::find_jj_dir_from(&user_workspace).context(
        "dojjo create must run inside a Jujutsu workspace (no .jj/repo found)",
    )?;

    if let Some((dojo_id, dojo_home)) = config::find_resumable_create(&user_workspace)? {
        if config::is_dojo_create_complete(&dojo_home) {
            println!(
                "dojo {dojo_id} is already set up (config {}). Run `dojjo dev sync` to publish changes.",
                config::dojo_config_path(&dojo_home).display()
            );
            return Ok(());
        }
        let ws_str = user_workspace
            .to_str()
            .context("workspace path must be UTF-8")?;
        let progress = match config::load_create_progress(&dojo_home)? {
            Some(p) => p,
            None => infer_create_progress(&user_workspace, &api_base, &dojo_id, &dojo_home)?,
        };
        anyhow::ensure!(
            progress.user_workspace == ws_str,
            "dojo {dojo_id} create was started from {}, not this workspace ({})",
            progress.user_workspace,
            ws_str
        );
        anyhow::ensure!(
            config::normalize_api_base(&progress.api_base) == api_base,
            "api_base {:?} does not match in-progress create {:?}",
            api_base,
            progress.api_base
        );
        println!("resuming dojjo create for {dojo_id} (phase {:?})…", progress.phase);
        return resume_create(client, &user_workspace, &dojo_home, &dojo_id, progress).await;
    }

    let user_name = config::default_user_workspace_name()?;
    let created = post_create_dojo(client, &api_base).await?;
    let dojo_id = created.id.clone();
    assert!(!dojo_id.is_empty(), "server must return non-empty dojo id");

    let dojo_home = config::dojo_home(&dojo_id)?;
    std::fs::create_dir_all(&dojo_home)
        .with_context(|| format!("create_dir_all {}", dojo_home.display()))?;

    let user_jj = config::find_jj_dir_from(&user_workspace)?;
    config::write_workspace_link(&user_jj, &dojo_id).context("write workspace link")?;

    let ws_str = user_workspace
        .to_str()
        .context("workspace path must be UTF-8")?;
    let progress = CreateProgress {
        phase: CreatePhase::ServerRegistered,
        user_workspace: ws_str.to_string(),
        api_base: api_base.clone(),
        git_remote_url: created.git_remote_url.clone(),
        user_workspace_name: user_name.clone(),
    };
    config::save_create_progress(&dojo_home, &progress).context("save create progress")?;

    resume_create(client, &user_workspace, &dojo_home, &dojo_id, progress).await
}

/// Recover phase for creates interrupted before `create_progress.json` existed.
fn infer_create_progress(
    user_workspace: &Path,
    api_base: &str,
    _dojo_id: &str,
    dojo_home: &Path,
) -> anyhow::Result<CreateProgress> {
    let user_jj = config::find_jj_dir_from(user_workspace)?;
    let user_name = config::default_user_workspace_name()?;
    let ws_str = user_workspace
        .to_str()
        .context("workspace path must be UTF-8")?;
    let phase = if config::sync_repo_root(dojo_home).is_err() {
        CreatePhase::ServerRegistered
    } else if config::workspace_rehomed_to_dojo(&user_jj, dojo_home)? {
        CreatePhase::Rehomed
    } else {
        CreatePhase::ServerRegistered
    };
    Ok(CreateProgress {
        phase,
        user_workspace: ws_str.to_string(),
        api_base: api_base.to_string(),
        git_remote_url: String::new(),
        user_workspace_name: user_name,
    })
}

async fn resume_create(
    client: &Client,
    user_workspace: &Path,
    dojo_home: &Path,
    dojo_id: &str,
    mut progress: CreateProgress,
) -> anyhow::Result<()> {
    assert!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let api_base = config::normalize_api_base(&progress.api_base);

    let git_remote_url = if progress.git_remote_url.is_empty() {
        let public = fetch_dojo_public(client, &api_base, dojo_id).await?;
        progress.git_remote_url = public.git_remote_url.clone();
        public.git_remote_url
    } else {
        progress.git_remote_url.clone()
    };
    assert!(
        !git_remote_url.is_empty(),
        "git_remote_url must not be empty"
    );

    let sync_repo = if progress.phase < CreatePhase::Rehomed {
        let sync_repo = workspace_lifecycle::ensure_dojo_after_create(
            user_workspace,
            dojo_home,
            &progress.user_workspace_name,
        )?;
        progress.phase = CreatePhase::Rehomed;
        config::save_create_progress(dojo_home, &progress).context("save create progress")?;
        sync_repo
    } else {
        config::sync_repo_root(dojo_home)?
    };
    assert!(
        sync_repo == config::sync_repo_root(dojo_home)?,
        "sync repo path must match dojo home _default host"
    );

    let git_dir = crate::git_sync::resolve_jj_backed_git_dir(&sync_repo)?;

    if progress.phase < CreatePhase::GitPushed {
        crate::git_sync::git_push_dojjo(&git_dir, &git_remote_url)
            .await
            .context("git push to dojjo bare remote (phase 1)")?;
        progress.phase = CreatePhase::GitPushed;
        config::save_create_progress(dojo_home, &progress).context("save create progress")?;
    }

    let paths = scan_repo_files(&sync_repo)?;
    let apply_order = compute_mirror_apply_order(&sync_repo, &paths).with_context(|| {
        format!(
            "compute apply order under {} (check repo integrity)",
            sync_repo.display()
        )
    })?;
    assert_eq!(
        apply_order.len(),
        paths.len(),
        "apply order is a permutation of scanned paths"
    );

    let man_before = fetch_manifest(client, &api_base, dojo_id).await?;
    let mut upload_count = 0usize;
    for rel in &apply_order {
        let abs = sync_repo.join(rel);
        let bytes = std::fs::read(&abs).with_context(|| format!("read {}", abs.display()))?;
        let sha = hex::encode(Sha256::digest(&bytes));
        if man_before
            .entries
            .get(rel)
            .is_some_and(|ent| ent.sha256 == sha && ent.size == bytes.len() as u64)
        {
            continue;
        }
        upload_count += 1;
        tus_upload_bytes(client, &api_base, dojo_id, rel, &bytes)
            .await
            .with_context(|| format!("upload {rel}"))?;
    }
    if upload_count > 0 {
        println!("uploaded {upload_count} file(s) to dojo {dojo_id}");
    } else if !apply_order.is_empty() {
        println!("mirror already up to date on server for dojo {dojo_id}");
    }

    let cfg = DojoConfig {
        dojo_id: dojo_id.to_string(),
        api_base: api_base.clone(),
        git_remote_url: Some(git_remote_url.clone()),
    };
    config::save_config(dojo_home, &cfg).context("save dojo config")?;

    let user_jj = config::find_jj_dir_from(user_workspace)?;
    config::write_workspace_link(&user_jj, dojo_id).context("write workspace dojjo link")?;

    let man = fetch_manifest(client, &api_base, dojo_id).await?;
    assert!(
        !man.revision.is_empty(),
        "manifest revision must not be empty"
    );
    for rel in &apply_order {
        let bytes = std::fs::read(sync_repo.join(rel))
            .with_context(|| format!("re-read {}", rel))?;
        let ent = man
            .entries
            .get(rel)
            .with_context(|| format!("manifest missing {rel}"))?;
        anyhow::ensure!(
            ent.size == bytes.len() as u64,
            "size mismatch for {rel} after upload"
        );
        let sha = hex::encode(Sha256::digest(&bytes));
        anyhow::ensure!(sha == ent.sha256, "sha mismatch for {rel} after upload");
    }

    config::save_state(
        dojo_home,
        &DojoState {
            last_remote_revision: man.revision.clone(),
        },
    )
    .context("save state")?;

    config::clear_create_progress(dojo_home).context("clear create progress")?;
    background_sync::ensure_worker_running(dojo_home).context("start background sync worker")?;

    println!("created dojo {dojo_id}; config {}", config::dojo_config_path(dojo_home).display());
    println!("sync repo {}", sync_repo.display());
    println!("user workspace name {}", progress.user_workspace_name);
    println!("manifest revision {}", man.revision);
    println!("{}", background_sync::join_or_create_status_message(dojo_home)?);
    Ok(())
}
