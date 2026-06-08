//! Push/pull the dojo local repo at `~/.dojjo/dojos/{id}/_default/.jj/repo`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context;
use dojjo_mirror::git_mirror_exclude::jj_repo_rel_git_transport_local_state;
use dojjo_mirror::mirror_exclude::jj_repo_rel_excluded_from_mirror;
use dojjo_mirror::mirror_apply_order::compute_mirror_apply_order;
use dojjo_mirror::operation_expand::expand_operation_parent_closure;
use reqwest::Client;

use crate::config::{self, DojoConfig};
use crate::jj_exec;
use crate::manifest::{fetch_manifest, Manifest};
use crate::repo_walk::sha256_index;
use crate::sync_debug::{self, PullCandidate, PulledRecord, SyncDebugRecord};
use crate::tus::tus_upload_bytes;
use crate::workspace_lifecycle;

pub async fn run(client: &Client, cwd: &Path) -> anyhow::Result<()> {
    assert!(cwd.as_os_str().len() > 0, "cwd must not be empty");
    let dojo_home = config::find_dojo_home_for_workspace(cwd)?;
    run_for_dojo_home(client, &dojo_home).await
}

pub async fn run_for_dojo_home(client: &Client, dojo_home: &Path) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let dojo_home = dojo_home
        .canonicalize()
        .with_context(|| format!("canonicalize {}", dojo_home.display()))?;
    let cfg = config::load_config(&dojo_home)?;
    let DojoConfig {
        dojo_id,
        api_base,
        git_remote_url,
    } = cfg;
    let api_base = config::normalize_api_base(&api_base);

    let jj_repo_folder = config::default_workspace_jj_repo_folder(&dojo_home)?;
    assert!(jj_repo_folder.is_dir(), "local repo must be a directory");

    let anchor_workspace = config::default_workspace_root(&dojo_home);
    let _default_workspace_root = workspace_lifecycle::ensure_default_workspace_loadable(&dojo_home, &anchor_workspace)?;

    workspace_lifecycle::ensure_local_repo_non_colocated_git(&jj_repo_folder)
        .context("non-colocated git store in local repo")?;

    let git_url = git_remote_url.as_ref().context(
        "missing git_remote_url in dojo config; re-run dojjo join or dojjo create with a current sync-server",
    )?;
    assert!(!git_url.is_empty(), "git_remote_url must not be empty when present");
    let git_dir = crate::git_sync::resolve_jj_backed_git_dir(&jj_repo_folder)?;

    let mut state = config::load_state(&dojo_home)?;

    // Composite-mode safety: `jj op log` reads the Git backend; heal a torn/corrupt git store first.
    if crate::git_sync::git_store_needs_healing(&git_dir).await? {
        crate::git_sync::git_fetch_dojjo(&git_dir, git_url)
            .await
            .context("heal corrupt git store (before jj op log)")?;
    }

    let workspace_op_id = jj_exec::workspace_current_operation_id_hex(&anchor_workspace)?;

    let man0 = fetch_manifest(client, &api_base, &dojo_id).await?;
    let local_at_start = sha256_index(&jj_repo_folder)?;
    assert!(
        local_at_start.len() < 500_000,
        "local file count must be bounded"
    );

    let git_need = crate::git_sync::git_transport_need(&git_dir, git_url).await?;
    let git_skipped = !git_need.push && !git_need.fetch;
    let git_ref_mismatch = if sync_debug::enabled() {
        let lines = crate::git_sync::git_ref_mismatch_lines(&git_dir, git_url).await?;
        if lines.is_empty() {
            None
        } else {
            Some(lines)
        }
    } else {
        None
    };
    let local_op_heads = sync_debug::list_op_head_ids(&jj_repo_folder);
    let server_op_heads: Vec<String> = man0
        .entries
        .keys()
        .filter_map(|p| p.strip_prefix("op_heads/heads/").map(|s| s.to_string()))
        .collect();
    sync_debug::emit(&SyncDebugRecord {
        phase: "start",
        workspace_op_id: Some(&workspace_op_id),
        man_revision: Some(&man0.revision),
        manifest_entries: Some(man0.entries.len()),
        git_skipped: Some(git_skipped),
        git_need_push: Some(git_need.push),
        git_need_fetch: Some(git_need.fetch),
        git_ref_mismatch,
        push_count: None,
        pull_count: None,
        pull_would_count: None,
        paths_changed: None,
        push_paths: None,
        pull_paths: None,
        pull_candidates: None,
        pulled_records: None,
        local_op_heads: Some(&local_op_heads),
        server_op_heads: Some(&server_op_heads),
        local_index_prefix_count: Some(sync_debug::index_prefix_count(
            &local_at_start,
            "store/extra/heads/",
        )),
    });

    crate::git_sync::git_push_dojjo(&git_dir, git_url)
        .await
        .context("git push to dojjo (before JJ mirror push)")?;

    let local_after_git_push = sha256_index(&jj_repo_folder)?;
    assert!(
        local_after_git_push.len() < 500_000,
        "local file count must be bounded"
    );
    let git_push_changed = sync_debug::index_changed_paths(&local_at_start, &local_after_git_push);
    sync_debug::emit(&SyncDebugRecord {
        phase: "after_git_push",
        workspace_op_id: Some(&workspace_op_id),
        man_revision: Some(&man0.revision),
        manifest_entries: None,
        git_skipped: Some(git_skipped),
        git_need_push: Some(git_need.push),
        git_need_fetch: Some(git_need.fetch),
        git_ref_mismatch: None,
        push_count: None,
        pull_count: None,
        pull_would_count: None,
        paths_changed: Some(&git_push_changed),
        push_paths: None,
        pull_paths: None,
        pull_candidates: None,
        pulled_records: None,
        local_op_heads: Some(&sync_debug::list_op_head_ids(&jj_repo_folder)),
        server_op_heads: None,
        local_index_prefix_count: None,
    });

    let mut push_paths: Vec<String> = Vec::new();
    for (path, (_len, sha)) in &local_after_git_push {
        match man0.entries.get(path) {
            None => push_paths.push(path.clone()),
            Some(ent) if ent.sha256 != *sha => push_paths.push(path.clone()),
            _ => {}
        }
    }
    push_paths.sort();

    if !push_paths.is_empty() {
        let push_needed: HashSet<String> = push_paths.iter().cloned().collect();
        let expanded =
            expand_operation_parent_closure(&jj_repo_folder, &push_paths).with_context(|| {
                format!(
                    "expand operation parent closure under {}",
                    jj_repo_folder.display()
                )
            })?;
        let order = compute_mirror_apply_order(&jj_repo_folder, &expanded)
            .with_context(|| format!("compute push apply order under {}", jj_repo_folder.display()))?;
        let upload_count = order.iter().filter(|p| push_needed.contains(*p)).count();
        println!("pushing {upload_count} changed file(s)…");
        for rel in &order {
            if !push_needed.contains(rel) {
                continue;
            }
            let bytes = std::fs::read(jj_repo_folder.join(rel))
                .with_context(|| format!("read {}", rel))?;
            tus_upload_bytes(client, &api_base, &dojo_id, rel, &bytes)
                .await
                .with_context(|| format!("upload {rel}"))?;
        }
    }

    crate::manifest::notify_sync_complete(client, &api_base, &dojo_id)
        .await
        .context("notify server mirror sync-complete (JJ normalize)")?;

    let mut man_published = fetch_manifest(client, &api_base, &dojo_id).await?;
    assert!(
        !man_published.revision.is_empty(),
        "published revision must not be empty"
    );
    let server_op_heads_published: Vec<String> = man_published
        .entries
        .keys()
        .filter_map(|p| p.strip_prefix("op_heads/heads/").map(|s| s.to_string()))
        .collect();
    sync_debug::emit(&SyncDebugRecord {
        phase: "after_sync_complete",
        workspace_op_id: Some(&workspace_op_id),
        man_revision: Some(&man_published.revision),
        manifest_entries: Some(man_published.entries.len()),
        git_skipped: Some(git_skipped),
        git_need_push: None,
        git_need_fetch: Some(git_need.fetch),
        git_ref_mismatch: None,
        push_count: None,
        pull_count: None,
        pull_would_count: None,
        paths_changed: None,
        push_paths: None,
        pull_paths: None,
        pull_candidates: None,
        pulled_records: None,
        local_op_heads: Some(&sync_debug::list_op_head_ids(&jj_repo_folder)),
        server_op_heads: Some(&server_op_heads_published),
        local_index_prefix_count: None,
    });

    crate::git_sync::git_fetch_dojjo(&git_dir, git_url)
        .await
        .context("git fetch from dojjo (before JJ mirror pull)")?;

    let local_after_git_fetch = sha256_index(&jj_repo_folder)?;
    assert!(
        local_after_git_fetch.len() < 500_000,
        "local file count must be bounded"
    );
    let git_fetch_changed =
        sync_debug::index_changed_paths(&local_after_git_push, &local_after_git_fetch);
    sync_debug::emit(&SyncDebugRecord {
        phase: "after_git_fetch",
        workspace_op_id: Some(&workspace_op_id),
        man_revision: Some(&man_published.revision),
        manifest_entries: Some(man_published.entries.len()),
        git_skipped: Some(git_skipped),
        git_need_push: None,
        git_need_fetch: Some(git_need.fetch),
        git_ref_mismatch: None,
        push_count: None,
        pull_count: None,
        pull_would_count: None,
        paths_changed: Some(&git_fetch_changed),
        push_paths: None,
        pull_paths: None,
        pull_candidates: None,
        pulled_records: None,
        local_op_heads: Some(&sync_debug::list_op_head_ids(&jj_repo_folder)),
        server_op_heads: None,
        local_index_prefix_count: None,
    });

    let local_op_heads_now = sync_debug::list_op_head_ids(&jj_repo_folder);
    let mut pull_candidates: Vec<PullCandidate<'_>> = Vec::new();
    let mut pull_set: HashSet<String> = HashSet::new();
    for p in &man_published.apply_order {
        if jj_repo_rel_excluded_from_mirror(p) {
            continue;
        }
        if jj_repo_rel_git_transport_local_state(p) {
            continue;
        }
        let ent = man_published
            .entries
            .get(p)
            .expect("apply_order ⊆ entries");
        let in_local_at_start = local_at_start.contains_key(p);
        let (local_sha, reason) = match local_after_git_fetch.get(p) {
            None => (None, "missing_local"),
            Some((_len, sha)) if sha != &ent.sha256 => (Some(sha.as_str()), "sha_mismatch"),
            Some((_len, sha)) => (Some(sha.as_str()), "already_match"),
        };
        let need = reason != "already_match";
        if sync_debug::enabled() && need {
            pull_candidates.push(PullCandidate {
                path: p,
                reason,
                in_local_at_start,
                local_sha,
                server_sha: &ent.sha256,
            });
        }
        if need {
            pull_set.insert(p.clone());
        }
    }

    sync_debug::emit(&SyncDebugRecord {
        phase: "pull_analysis",
        workspace_op_id: Some(&workspace_op_id),
        man_revision: Some(&man_published.revision),
        manifest_entries: Some(man_published.entries.len()),
        git_skipped: None,
        git_need_push: None,
        git_need_fetch: Some(git_need.fetch),
        git_ref_mismatch: None,
        push_count: None,
        pull_count: Some(pull_set.len()),
        pull_would_count: Some(pull_set.len()),
        paths_changed: None,
        push_paths: None,
        pull_paths: None,
        pull_candidates: if pull_candidates.is_empty() {
            None
        } else {
            Some(&pull_candidates)
        },
        pulled_records: None,
        local_op_heads: Some(&local_op_heads_now),
        server_op_heads: Some(&server_op_heads_published),
        local_index_prefix_count: Some(sync_debug::index_prefix_count(
            &local_after_git_fetch,
            "store/extra/heads/",
        )),
    });

    let mut pulled_records: Vec<PulledRecord> = Vec::new();
    if !pull_set.is_empty() {
        println!("pulling {} file(s)…", pull_set.len());
        let pull_order = man_published.apply_order.clone();
        for rel in pull_order {
            if jj_repo_rel_excluded_from_mirror(&rel) {
                continue;
            }
            if jj_repo_rel_git_transport_local_state(&rel) {
                continue;
            }
            if !pull_set.contains(&rel) {
                continue;
            }
            let in_local_at_start = local_at_start.contains_key(&rel);
            let Some(bytes) = crate::manifest::get_mirror_object_bytes(
                client,
                &api_base,
                &dojo_id,
                &rel,
                &mut man_published,
            )
            .await? else {
                continue;
            };
            let ent = man_published
                .entries
                .get(&rel)
                .expect("entry after successful GET");

            let dest = jj_repo_folder.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create_dir_all {}", parent.display()))?;
            }
            std::fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;

            if sync_debug::enabled() {
                pulled_records.push(PulledRecord {
                    path: rel,
                    in_local_at_start,
                    server_sha: ent.sha256.clone(),
                });
            }
        }
    }

    sync_debug::emit(&SyncDebugRecord {
        phase: "done",
        workspace_op_id: Some(&workspace_op_id),
        man_revision: Some(&man_published.revision),
        manifest_entries: None,
        git_skipped: Some(git_skipped),
        git_need_push: None,
        git_need_fetch: None,
        git_ref_mismatch: None,
        push_count: None,
        pull_count: Some(pull_set.len()),
        pull_would_count: Some(pull_set.len()),
        paths_changed: None,
        push_paths: Some(&push_paths),
        pull_paths: Some(&pull_set.iter().cloned().collect::<Vec<_>>()),
        pull_candidates: None,
        pulled_records: if pulled_records.is_empty() {
            None
        } else {
            Some(&pulled_records)
        },
        local_op_heads: Some(&sync_debug::list_op_head_ids(&jj_repo_folder)),
        server_op_heads: None,
        local_index_prefix_count: None,
    });

    let prune_count = prune_local_mirror_paths_not_in_manifest(&jj_repo_folder, &man_published)?;
    if prune_count > 0 {
        println!("pruned {prune_count} local file(s) not on server manifest");
    }
    verify_local_matches_manifest(&sha256_index(&jj_repo_folder)?, &man_published)?;

    state.last_remote_revision = man_published.revision.clone();
    config::save_state(&dojo_home, &state).context("save state")?;

    println!(
        "sync complete; manifest revision {}",
        man_published.revision
    );
    Ok(())
}

fn verify_local_matches_manifest(
    local_index: &HashMap<String, (u64, String)>,
    manifest: &Manifest,
) -> anyhow::Result<()> {
    assert!(!manifest.revision.is_empty(), "revision must not be empty");
    for (p, ent) in &manifest.entries {
        if jj_repo_rel_excluded_from_mirror(p) {
            continue;
        }
        if jj_repo_rel_git_transport_local_state(p) {
            continue;
        }
        let got = local_index
            .get(p)
            .with_context(|| format!("after sync, local missing server path {p}"))?;
        anyhow::ensure!(got.1 == ent.sha256, "after sync sha mismatch for {p}");
        anyhow::ensure!(got.0 == ent.size, "after sync size mismatch for {p}");
    }
    for p in local_index.keys() {
        if jj_repo_rel_excluded_from_mirror(p) {
            continue;
        }
        if jj_repo_rel_git_transport_local_state(p) {
            continue;
        }
        assert!(
            manifest.entries.contains_key(p),
            "after sync local must not have unknown path not on server: {p}"
        );
    }
    Ok(())
}

fn prune_local_mirror_paths_not_in_manifest(
    jj_repo_folder: &Path,
    manifest: &Manifest,
) -> anyhow::Result<usize> {
    assert!(jj_repo_folder.as_os_str().len() > 0, "jj_repo_folder must not be empty");
    assert!(!manifest.revision.is_empty(), "revision must not be empty");
    let local_index = sha256_index(jj_repo_folder)?;
    let mut pruned = 0usize;
    for p in local_index.keys() {
        if jj_repo_rel_excluded_from_mirror(p) {
            continue;
        }
        if jj_repo_rel_git_transport_local_state(p) {
            continue;
        }
        if manifest.entries.contains_key(p) {
            continue;
        }
        let abs = jj_repo_folder.join(p);
        assert!(abs.starts_with(jj_repo_folder), "path must stay under jj_repo_folder");
        assert!(abs.is_file(), "prune target must be a regular file: {p}");
        std::fs::remove_file(&abs)
            .with_context(|| format!("prune local {p} not on server manifest"))?;
        pruned += 1;
    }
    Ok(pruned)
}
