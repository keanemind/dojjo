//! Resolve JJ's Git directory and run `git fetch` / `git push` against the per-dojo bare remote.
//!
//! Remotes are `file://` (local sync-server) or `http://` / `https://` smart HTTP from the API.
//! Configure the `dojjo` remote and let Git move objects and refs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::process::Command;

/// Resolve the Git repository directory JJ uses (non-colocated: `store/git/` via `git_target = "git"`).
pub fn resolve_jj_backed_git_dir(repo_root: &Path) -> anyhow::Result<PathBuf> {
    assert!(
        repo_root.as_os_str().len() > 0,
        "repo_root must be non-empty path"
    );
    assert!(repo_root.is_dir(), "repo_root must be a directory");

    let store = repo_root.join("store");
    assert!(
        store.starts_with(repo_root),
        "store must live under repo root"
    );
    let target_file = store.join("git_target");
    assert!(
        target_file.is_file(),
        "git_target must exist at {}",
        target_file.display()
    );

    let raw = std::fs::read_to_string(&target_file)
        .with_context(|| format!("read {}", target_file.display()))?;
    let trimmed = raw.trim();
    anyhow::ensure!(
        !trimmed.is_empty(),
        "git_target must not be empty at {}",
        target_file.display()
    );
    anyhow::ensure!(
        trimmed == "git",
        "dojo sync hosts use internal git store (git_target must be \"git\", got {trimmed:?} at {})",
        target_file.display()
    );

    let joined = store.join(trimmed);
    let canon = joined
        .canonicalize()
        .with_context(|| format!("canonicalize git dir from {}", joined.display()))?;
    assert!(
        canon.as_os_str().len() > 0,
        "canonical git dir must be non-empty"
    );
    assert!(
        canon.is_dir(),
        "resolved git path must be a directory (got {})",
        canon.display()
    );

    let head = canon.join("HEAD");
    assert!(
        head.is_file(),
        "resolved git dir must contain HEAD at {}",
        head.display()
    );

    Ok(canon)
}

/// Spawn `git` without inherited `GIT_DIR` / `GIT_WORK_TREE` (would hijack init/fetch/push).
fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env_remove("GIT_DIR");
    cmd.env_remove("GIT_WORK_TREE");
    cmd.env_remove("GIT_INDEX_FILE");
    cmd
}

/// Ensure `git_dir` is a valid bare Git repository (creates via `git init --bare` when missing).
pub fn ensure_git_dir_layout(git_dir: &Path) -> anyhow::Result<()> {
    let head = git_dir.join("HEAD");
    let config = git_dir.join("config");
    if !head.is_file() || !config.is_file() {
        if git_dir.exists() {
            assert!(git_dir.is_dir(), "git_dir must be a directory");
        }
        let git_dir_str = git_dir
            .to_str()
            .context("git_dir path must be UTF-8 for git init")?;
        let status = {
            let mut cmd = std::process::Command::new("git");
            cmd.env_remove("GIT_DIR");
            cmd.env_remove("GIT_WORK_TREE");
            cmd.env_remove("GIT_INDEX_FILE");
            cmd.arg("init").arg("--bare").arg(git_dir_str);
            cmd.status()
                .with_context(|| format!("git init --bare {}", git_dir.display()))?
        };
        anyhow::ensure!(
            status.success(),
            "git init --bare failed for {}",
            git_dir.display()
        );
        assert!(head.is_file(), "git init must create HEAD");
        assert!(config.is_file(), "git init must create config");
    }
    assert!(git_dir.is_dir(), "git_dir must be a directory");
    let objects = git_dir.join("objects");
    if !objects.exists() {
        std::fs::create_dir_all(objects.join("info"))
            .with_context(|| format!("create {}", objects.display()))?;
    }
    // Git treats missing `refs/` as "not a repository" in some commands. Cold-join mirror pulls
    // can omit empty directories, so ensure the directory exists before `git remote add` / fetch.
    let refs = git_dir.join("refs");
    if !refs.exists() {
        std::fs::create_dir_all(refs.join("heads"))
            .with_context(|| format!("create {}", refs.display()))?;
        std::fs::create_dir_all(refs.join("tags"))
            .with_context(|| format!("create {}", refs.display()))?;
    }
    Ok(())
}

async fn git_output(git_dir: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    assert!(git_dir.is_dir(), "git_dir must exist");
    assert!(!args.is_empty(), "git args must not be empty");
    ensure_git_dir_layout(git_dir)?;

    let git_dir = git_dir
        .to_str()
        .context("git_dir path must be UTF-8 for --git-dir")?;

    let out = git_command()
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    assert!(
        out.status.code().is_some(),
        "git must exit with a status code"
    );
    Ok(out)
}

fn assert_supported_git_remote_url(remote_url: &str) {
    assert!(!remote_url.is_empty(), "remote_url must not be empty");
    let ok = remote_url.starts_with("file://")
        || remote_url.starts_with("http://")
        || remote_url.starts_with("https://");
    assert!(ok, "remote_url must be file:// or http(s):// (got {remote_url:?})");
    assert!(
        !remote_url.contains('@'),
        "remote_url must not use user@host scp/ssh form"
    );
}

/// Ensure remote name `dojjo` points at `remote_url`.
pub async fn ensure_dojjo_remote(git_dir: &Path, remote_url: &str) -> anyhow::Result<()> {
    assert!(git_dir.is_dir(), "git_dir must be a directory");
    assert_supported_git_remote_url(remote_url);

    let probe = git_output(git_dir, &["remote", "get-url", "dojjo"]).await?;
    if probe.status.success() {
        let existing = String::from_utf8_lossy(&probe.stdout);
        let existing = existing.trim();
        assert!(
            !existing.is_empty(),
            "git remote get-url must return non-empty"
        );
        if existing == remote_url {
            return Ok(());
        }
        let set = git_output(git_dir, &["remote", "set-url", "dojjo", remote_url]).await?;
        anyhow::ensure!(
            set.status.success(),
            "git remote set-url dojjo failed: {}",
            String::from_utf8_lossy(&set.stderr)
        );
        return Ok(());
    }

    let add = git_output(git_dir, &["remote", "add", "dojjo", remote_url]).await?;
    anyhow::ensure!(
        add.status.success(),
        "git remote add dojjo failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    Ok(())
}

/// Remove the `dojjo` remote if present.
pub async fn remove_dojjo_remote(git_dir: &Path) -> anyhow::Result<()> {
    assert!(git_dir.is_dir(), "git_dir must be a directory");
    let probe = git_output(git_dir, &["remote", "get-url", "dojjo"]).await?;
    if !probe.status.success() {
        return Ok(());
    }
    let rm = git_output(git_dir, &["remote", "remove", "dojjo"]).await?;
    anyhow::ensure!(
        rm.status.success(),
        "git remote remove dojjo failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );
    Ok(())
}

/// Whether the Git leg must run before JJ mirror push/pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitTransportNeed {
    pub push: bool,
    pub fetch: bool,
}

/// `true` when the local git object database or refs fail `git fsck` and need a fetch to heal.
pub async fn git_store_needs_healing(git_dir: &Path) -> anyhow::Result<bool> {
    assert!(git_dir.is_dir(), "git_dir must be a directory");
    let fsck = git_output(git_dir, &["fsck", "--no-progress"]).await?;
    Ok(!fsck.status.success())
}

/// Compare local refs against `dojjo` on the server. When both are false, skip `git push` / `git fetch`
/// so idle sync does not rewrite mirrored paths via `checkout` / ref updates.
pub async fn git_transport_need(git_dir: &Path, remote_url: &str) -> anyhow::Result<GitTransportNeed> {
    assert!(git_dir.is_dir(), "git_dir must be a directory");
    assert_supported_git_remote_url(remote_url);
    ensure_dojjo_remote(git_dir, remote_url).await?;

    let remote = git_ls_remote_map(git_dir, "dojjo").await?;
    let local = git_local_ref_map(git_dir).await?;

    let mut need_push = false;
    let mut need_fetch = false;

    for (ref_name, local_oid) in &local.push_refs {
        match remote.get(ref_name) {
            None => {
                need_push = true;
            }
            Some(remote_oid) if remote_oid != local_oid => {
                need_push = true;
            }
            _ => {}
        }
    }

    for (ref_name, remote_oid) in &remote.fetch_refs {
        let local_oid = local.fetch_ref(ref_name);
        if local_oid != Some(remote_oid.as_str()) {
            need_fetch = true;
        }
    }

    // If refs are aligned but the local object database is corrupt/missing objects (e.g. torn pack,
    // or `refs/jj/keep/*` pointing at missing commits), we still must fetch to heal.
    // Do not use `--connectivity-only`: it skips unreachable refs such as `refs/jj/keep/*`.
    if !need_fetch {
        let fsck = git_output(git_dir, &["fsck", "--no-progress"]).await?;
        if !fsck.status.success() {
            need_fetch = true;
        }
    }

    Ok(GitTransportNeed {
        push: need_push,
        fetch: need_fetch,
    })
}

/// Ref-by-ref comparison for debug (`local` / `remote` oid hex, or `"-"` if missing).
pub async fn git_ref_mismatch_lines(
    git_dir: &Path,
    remote_url: &str,
) -> anyhow::Result<Vec<(String, String, String)>> {
    assert!(git_dir.is_dir(), "git_dir must be a directory");
    assert_supported_git_remote_url(remote_url);
    ensure_dojjo_remote(git_dir, remote_url).await?;

    let remote = git_ls_remote_map(git_dir, "dojjo").await?;
    let local = git_local_ref_map(git_dir).await?;

    let mut names = BTreeSet::new();
    for k in local.push_refs.keys() {
        names.insert(k.clone());
    }
    for k in remote.fetch_refs.keys() {
        names.insert(k.clone());
    }

    let mut out = Vec::new();
    for ref_name in &names {
        let local_oid = local
            .fetch_ref(ref_name)
            .or_else(|| local.push_refs.get(ref_name).map(|s| s.as_str()))
            .unwrap_or("-");
        let remote_oid = remote.get(ref_name).map(|s| s.as_str()).unwrap_or("-");
        if local_oid != remote_oid {
            out.push((
                ref_name.clone(),
                local_oid.to_string(),
                remote_oid.to_string(),
            ));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

struct LocalRefMap {
    /// `refs/heads/*` and `refs/jj/*` for push comparison.
    push_refs: BTreeMap<String, String>,
}

impl LocalRefMap {
    fn fetch_ref(&self, ref_name: &str) -> Option<&str> {
        if let Some(oid) = self.push_refs.get(ref_name) {
            return Some(oid.as_str());
        }
        let tracking = ref_name.replacen("refs/heads/", "refs/remotes/dojjo/", 1);
        self.push_refs
            .get(&tracking)
            .map(|s| s.as_str())
    }
}

async fn git_ls_remote_map(git_dir: &Path, remote: &str) -> anyhow::Result<RemoteRefMap> {
    let out = git_output(git_dir, &["ls-remote", "--refs", remote]).await?;
    anyhow::ensure!(
        out.status.success(),
        "git ls-remote failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(RemoteRefMap::parse_stdout(&out.stdout))
}

struct RemoteRefMap {
    /// Refs we may need to fetch (heads + jj + tags).
    fetch_refs: BTreeMap<String, String>,
}

impl RemoteRefMap {
    fn parse_stdout(stdout: &[u8]) -> Self {
        let mut fetch_refs = BTreeMap::new();
        for line in stdout.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let line = std::str::from_utf8(line).expect("ls-remote output must be utf-8");
            let Some((oid, ref_name)) = line.split_once('\t') else {
                continue;
            };
            if ref_name.starts_with("refs/heads/")
                || ref_name.starts_with("refs/jj/")
                || ref_name.starts_with("refs/tags/")
            {
                fetch_refs.insert(ref_name.to_string(), oid.to_string());
            }
        }
        Self { fetch_refs }
    }

    fn get(&self, ref_name: &str) -> Option<&String> {
        self.fetch_refs.get(ref_name)
    }
}

async fn git_local_ref_map(git_dir: &Path) -> anyhow::Result<LocalRefMap> {
    let out = git_output(
        git_dir,
        &[
            "for-each-ref",
            "refs/heads",
            "refs/jj",
            "refs/remotes/dojjo",
            "refs/tags",
            "--format=%(refname)\t%(objectname)",
        ],
    )
    .await?;
    anyhow::ensure!(
        out.status.success(),
        "git for-each-ref failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut push_refs = BTreeMap::new();
    for line in out.stdout.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(line).expect("for-each-ref output must be utf-8");
        let Some((ref_name, oid)) = line.split_once('\t') else {
            continue;
        };
        if ref_name.starts_with("refs/remotes/dojjo/") {
            continue;
        }
        if ref_name.starts_with("refs/heads/")
            || ref_name.starts_with("refs/jj/")
            || ref_name.starts_with("refs/tags/")
        {
            push_refs.insert(ref_name.to_string(), oid.to_string());
        }
    }

    // Tracking refs for fetch comparison (not pushed directly).
    for line in out.stdout.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(line).expect("for-each-ref output must be utf-8");
        let Some((ref_name, oid)) = line.split_once('\t') else {
            continue;
        };
        if ref_name.starts_with("refs/remotes/dojjo/") {
            push_refs.insert(ref_name.to_string(), oid.to_string());
        }
    }

    Ok(LocalRefMap { push_refs })
}

/// Push all refs to the dojjo bare remote (Git transport for object database + refs).
pub async fn git_push_dojjo(git_dir: &Path, remote_url: &str) -> anyhow::Result<()> {
    ensure_dojjo_remote(git_dir, remote_url).await?;
    let out = git_output(
        git_dir,
        &["push", "--no-progress", "dojjo", "+refs/*:refs/*"],
    )
    .await?;
    anyhow::ensure!(
        out.status.success(),
        "git push to dojjo failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// Refspecs for fetch into a local repo (may have `main` checked out in colocated `_default`).
///
/// Do not use `+refs/*:refs/*` — that maps `refs/heads/main` onto the checked-out branch and Git
/// refuses. Heads land under `refs/remotes/dojjo/*`; `align_head_with_dojjo_remote` checks out from
/// there. Push to the bare remote still uses `+refs/*:refs/*` (no worktree on the server).
const GIT_FETCH_DOJJO_REFSPECS: &[&str] = &[
    "+refs/heads/*:refs/remotes/dojjo/*",
    "+refs/tags/*:refs/tags/*",
    "+refs/jj/*:refs/jj/*",
];

/// Fetch from the dojjo bare remote into this repository.
pub async fn git_fetch_dojjo(git_dir: &Path, remote_url: &str) -> anyhow::Result<()> {
    ensure_dojjo_remote(git_dir, remote_url).await?;
    // If the local git store is corrupt (e.g. torn pack), fetch negotiation may decide it's
    // up-to-date because ref tips match, and then skip re-downloading missing objects. Heal by
    // retrying with a hard reset of `objects/` + `refs/` if needed.
    for attempt in 0..2 {
        // Full fsck (not `--connectivity-only`): torn packs and dangling `refs/jj/keep/*` must
        // trigger `--refetch`, otherwise negotiation may skip object transfer.
        let pre_fsck = git_output(git_dir, &["fsck", "--no-progress"]).await?;
        let refetch = !pre_fsck.status.success() || attempt > 0;
        if refetch {
            let objects = git_dir.join("objects");
            if objects.exists() {
                std::fs::remove_dir_all(&objects)
                    .with_context(|| format!("remove {}", objects.display()))?;
            }
            let refs = git_dir.join("refs");
            if refs.exists() {
                std::fs::remove_dir_all(&refs)
                    .with_context(|| format!("remove {}", refs.display()))?;
            }
            let packed = git_dir.join("packed-refs");
            if packed.exists() {
                std::fs::remove_file(&packed)
                    .with_context(|| format!("remove {}", packed.display()))?;
            }
        }

        let mut args = vec!["fetch", "--prune", "--no-progress"];
        if refetch {
            // Forces object transfer even if ref tips match (critical for healing torn packs).
            args.push("--refetch");
        }
        args.push("dojjo");
        args.extend_from_slice(GIT_FETCH_DOJJO_REFSPECS);
        let out = git_output(git_dir, &args).await?;
        anyhow::ensure!(
            out.status.success(),
            "git fetch from dojjo failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let post_fsck = git_output(git_dir, &["fsck", "--no-progress"]).await?;
        if post_fsck.status.success() {
            break;
        }
        if attempt == 1 {
            anyhow::bail!(
                "git fetch succeeded but local git store still corrupt (fsck failed)"
            );
        }
    }

    align_head_with_dojjo_remote(git_dir).await?;
    Ok(())
}

/// After fetch, check out the branch we received from `dojjo` so colocated `.git/HEAD` matches peers.
async fn align_head_with_dojjo_remote(git_dir: &Path) -> anyhow::Result<()> {
    assert!(git_dir.is_dir(), "git_dir must be a directory");
    for branch in ["main", "master"] {
        let remote_ref = format!("refs/remotes/dojjo/{branch}");
        let probe = git_output(git_dir, &["show-ref", "--verify", &remote_ref]).await?;
        if !probe.status.success() {
            continue;
        }
        let co = git_output(
            git_dir,
            &["checkout", "-B", branch, &format!("dojjo/{branch}")],
        )
        .await?;
        if co.status.success() {
            return Ok(());
        }
    }
    Ok(())
}
