//! Enumerate regular files under `.jj/repo` for mirror upload (skips `.dojjo/`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use dojjo_mirror::git_mirror_exclude::jj_repo_rel_git_transport_local_state;
use dojjo_mirror::mirror_exclude::jj_repo_rel_excluded_from_mirror;
use dojjo_mirror::mirror_path::parse_safe_mirror_rel;
use sha2::{Digest, Sha256};

const SKIP_UNDER_REPO: &str = ".dojjo";

fn path_to_slash(rel: &Path) -> anyhow::Result<String> {
    assert!(
        rel.components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "rel path must contain only normal components"
    );
    let mut out = String::new();
    for (i, c) in rel.iter().enumerate() {
        assert!(i < 10_000, "path depth must be bounded");
        if i > 0 {
            out.push('/');
        }
        let s = c
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 path segment under repo"))?;
        assert!(!s.is_empty(), "segment must not be empty");
        out.push_str(s);
    }
    assert!(!out.is_empty(), "non-empty rel must yield non-empty slash path");
    Ok(out)
}

/// All regular files under `repo_root`, as slash-separated paths relative to `repo_root`.
/// Skips the `.dojjo/` subtree. Every path must pass [`parse_safe_mirror_rel`].
pub fn scan_repo_files(repo_root: &Path) -> anyhow::Result<Vec<String>> {
    assert!(
        repo_root.as_os_str().len() > 0,
        "repo_root must be non-empty path"
    );
    assert!(repo_root.is_dir(), "repo_root must be a directory");

    let mut stack = vec![PathBuf::new()];
    let mut paths = Vec::new();
    let mut seen = 0usize;

    while let Some(rel) = stack.pop() {
        assert!(seen < 200_000, "repo file count must stay bounded");
        let abs = repo_root.join(&rel);
        assert!(
            abs.starts_with(repo_root),
            "walk must stay under repo root"
        );
        for ent in std::fs::read_dir(&abs).with_context(|| format!("read_dir {}", abs.display()))? {
            seen += 1;
            let ent = ent?;
            let name = ent.file_name();
            if rel.components().next().is_none() && name == SKIP_UNDER_REPO {
                continue;
            }
            let ft = ent.file_type().context("file_type")?;
            // JJ secure-config uses `config.toml` -> ~/.config/jj/repos/<id>/config.toml;
            // other symlinks are git glue or host-local — never mirror, git push handles git store.
            if ft.is_symlink() {
                continue;
            }
            let mut child = rel.clone();
            child.push(&name);
            if ft.is_dir() {
                stack.push(child);
                continue;
            }
            if !ft.is_file() {
                anyhow::bail!("unsupported file type under {}", abs.display());
            }
            let slash = path_to_slash(&child)?;
            if parse_safe_mirror_rel(&slash).is_err() {
                anyhow::bail!(
                    "path is not a valid mirror rel (check dojjo-mirror rules): {slash}"
                );
            }
            if jj_repo_rel_excluded_from_mirror(&slash) {
                continue;
            }
            if jj_repo_rel_git_transport_local_state(&slash) {
                continue;
            }
            paths.push(slash);
        }
    }

    paths.sort();
    assert!(
        paths.windows(2).all(|w| w[0] < w[1]),
        "sorted paths must be strictly increasing"
    );
    Ok(paths)
}

/// SHA256 hex (64 chars) and size for each file under `repo_root` (same rules as [`scan_repo_files`]).
pub fn sha256_index(repo_root: &Path) -> anyhow::Result<HashMap<String, (u64, String)>> {
    let paths = scan_repo_files(repo_root)?;
    let mut out = HashMap::with_capacity(paths.len());
    for p in paths {
        let abs = repo_root.join(&p);
        assert!(abs.starts_with(repo_root), "file must stay under repo");
        let bytes = std::fs::read(&abs).with_context(|| format!("read {}", abs.display()))?;
        let len = bytes.len() as u64;
        let sha = hex::encode(Sha256::digest(&bytes));
        assert_eq!(sha.len(), 64, "sha256 hex length");
        out.insert(p, (len, sha));
    }
    Ok(out)
}
