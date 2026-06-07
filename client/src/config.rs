//! Dojjo paths, dojo config under `~/.dojjo/dojos/{id}/`, and JJ repo pointer resolution.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const GLOBAL_CONFIG_FILE: &str = "config.json";
pub const CONFIG_FILE: &str = "config.json";
pub const STATE_FILE: &str = "state.json";
pub const REPO_MACHINE_FILE: &str = "repo_machine.json";
pub const CREATE_PROGRESS_FILE: &str = "create_progress.json";

/// Global client settings under `~/.dojjo/config.json` (set by `dojjo init`).
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct GlobalConfig {
    pub api_base: String,
}
/// Hidden JJ workspace that hosts the physical `.jj/repo` (sync target).
pub const DEFAULT_WORKSPACE_DIR: &str = "_default";
/// JJ's reserved default workspace name in the shared view.
pub const DEFAULT_WORKSPACE_NAME: &str = "default";
/// Per-user workspace marker (under workspace `.jj/`).
pub const DOJJO_LINK_FILE: &str = "dojjo.json";

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct DojoConfig {
    pub dojo_id: String,
    pub api_base: String,
    /// Bare Git remote: `file://` (local dev) or `user@host:/path` (SSH / VPS).
    #[serde(default)]
    pub git_remote_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct DojoLink {
    pub dojo_id: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct DojoState {
    #[serde(default)]
    pub last_remote_revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct RepoMachineConfig {
    #[serde(default = "repo_machine_background_sync_enabled_default")]
    pub background_sync_enabled: bool,
}

fn repo_machine_background_sync_enabled_default() -> bool {
    true
}

impl Default for RepoMachineConfig {
    fn default() -> Self {
        Self {
            background_sync_enabled: repo_machine_background_sync_enabled_default(),
        }
    }
}

/// Resumable `dojjo create` checkpoints (written after each phase completes).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub enum CreatePhase {
    ServerRegistered,
    Rehomed,
    GitPushed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct CreateProgress {
    pub phase: CreatePhase,
    /// Canonical path to the user's project workspace when create started.
    pub user_workspace: String,
    pub api_base: String,
    pub git_remote_url: String,
    pub user_workspace_name: String,
}

pub fn dojo_create_progress_path(dojo_home: &Path) -> PathBuf {
    dojo_home.join(CREATE_PROGRESS_FILE)
}

pub fn save_create_progress(dojo_home: &Path, progress: &CreateProgress) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    assert!(
        !progress.user_workspace.is_empty(),
        "user_workspace must not be empty"
    );
    assert!(!progress.api_base.is_empty(), "api_base must not be empty");
    assert!(
        !progress.git_remote_url.is_empty(),
        "git_remote_url must not be empty"
    );
    std::fs::create_dir_all(dojo_home)
        .with_context(|| format!("create_dir_all {}", dojo_home.display()))?;
    let path = dojo_create_progress_path(dojo_home);
    let json = serde_json::to_vec_pretty(progress).context("serialize create progress")?;
    atomic_write(&path, &json).context("write create progress")?;
    Ok(())
}

pub fn load_create_progress(dojo_home: &Path) -> anyhow::Result<Option<CreateProgress>> {
    let path = dojo_create_progress_path(dojo_home);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    assert!(!bytes.is_empty(), "create progress must not be empty when present");
    let p: CreateProgress = serde_json::from_slice(&bytes).context("parse create progress")?;
    Ok(Some(p))
}

pub fn clear_create_progress(dojo_home: &Path) -> anyhow::Result<()> {
    let path = dojo_create_progress_path(dojo_home);
    if path.is_file() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// True when `config.json` exists (create finished successfully).
pub fn is_dojo_create_complete(dojo_home: &Path) -> bool {
    dojo_config_path(dojo_home).is_file()
}

/// User `.jj/repo` is a pointer at the sync host for this dojo.
pub fn workspace_rehomed_to_dojo(user_jj: &Path, dojo_home: &Path) -> anyhow::Result<bool> {
    let repo_path = user_jj.join("repo");
    if repo_path.is_dir() {
        return Ok(false);
    }
    if !repo_path.is_file() {
        return Ok(false);
    }
    let canon = resolve_repo_path_at_jj(user_jj)?;
    let sync_repo = sync_repo_root(dojo_home)?;
    Ok(canon == sync_repo)
}

/// Find an in-progress create for this workspace (link file or `create_progress.json` scan).
pub fn find_resumable_create(
    user_workspace: &Path,
) -> anyhow::Result<Option<(String, PathBuf)>> {
    let user_workspace = user_workspace
        .canonicalize()
        .with_context(|| format!("canonicalize {}", user_workspace.display()))?;
    let ws_str = user_workspace
        .to_str()
        .context("workspace path must be UTF-8")?;

    let jj_dir = find_jj_dir_from(&user_workspace)?;
    if let Some(link) = read_workspace_link(&jj_dir)? {
        let home = dojo_home(&link.dojo_id)?;
        if is_dojo_create_complete(&home) {
            return Ok(Some((link.dojo_id, home)));
        }
        if load_create_progress(&home)?.is_some() {
            return Ok(Some((link.dojo_id, home)));
        }
        if workspace_rehomed_to_dojo(&jj_dir, &home)? || sync_repo_root(&home).is_ok() {
            return Ok(Some((link.dojo_id, home)));
        }
    }

    let base = dojjo_home_base()?;
    let dojos_dir = base.join("dojos");
    if !dojos_dir.is_dir() {
        return Ok(None);
    }
    for ent in std::fs::read_dir(&dojos_dir).with_context(|| format!("read_dir {}", dojos_dir.display()))? {
        let ent = ent?;
        if !ent.file_type().context("file_type")?.is_dir() {
            continue;
        }
        let home = ent.path();
        if is_dojo_create_complete(&home) {
            continue;
        }
        let Some(progress) = load_create_progress(&home)? else {
            continue;
        };
        if progress.user_workspace == ws_str {
            let id = ent.file_name();
            let id = id.to_str().context("dojo id must be UTF-8")?;
            return Ok(Some((id.to_string(), home)));
        }
    }
    Ok(None)
}

pub fn normalize_api_base(api_base: &str) -> String {
    let s = api_base.trim_end_matches('/');
    assert!(!s.is_empty() || api_base.is_empty(), "trim invariant");
    s.to_string()
}

/// Base directory for all dojos (`~/.dojjo` or `$DOJJO_HOME`).
pub fn global_config_path() -> anyhow::Result<PathBuf> {
    Ok(dojjo_home_base()?.join(GLOBAL_CONFIG_FILE))
}

/// Write `path` atomically (temp file in the same directory, then `rename`).
pub fn atomic_write(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    assert!(path.as_os_str().len() > 0, "path must not be empty");
    assert!(!data.is_empty(), "data must not be empty");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create_dir_all {}", parent.display()))?;
        }
    }
    let tmp = path.with_extension("tmp");
    assert!(
        tmp.parent() == path.parent(),
        "temp file must live beside destination for atomic rename"
    );
    std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn load_global_config() -> anyhow::Result<GlobalConfig> {
    let path = global_config_path()?;
    if !path.is_file() {
        anyhow::bail!("missing global config at {}", path.display());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    assert!(!bytes.is_empty(), "global config must not be empty");
    let c: GlobalConfig = serde_json::from_slice(&bytes).context("parse global config")?;
    assert!(!c.api_base.is_empty(), "api_base in global config must not be empty");
    Ok(c)
}

pub fn save_global_config(cfg: &GlobalConfig) -> anyhow::Result<()> {
    assert!(!cfg.api_base.is_empty(), "api_base must not be empty");
    let path = global_config_path()?;
    let json = serde_json::to_vec_pretty(cfg).context("serialize global config")?;
    atomic_write(&path, &json).context("write global config")?;
    Ok(())
}

/// Load global config or return a clear error pointing at `dojjo init`.
pub fn require_global_config() -> anyhow::Result<GlobalConfig> {
    let path = global_config_path()?;
    if !path.is_file() {
        anyhow::bail!(
            "dojjo is not configured (no {}). Run `dojjo init` first.",
            path.display()
        );
    }
    load_global_config()
}

/// Global `api_base`, with optional per-command override.
pub fn resolve_api_base(override_base: Option<&str>) -> anyhow::Result<String> {
    if let Some(s) = override_base {
        let s = normalize_api_base(s);
        anyhow::ensure!(!s.is_empty(), "--api-base must not be empty");
        return Ok(s);
    }
    let cfg = require_global_config()?;
    Ok(normalize_api_base(&cfg.api_base))
}

pub fn dojjo_home_base() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("DOJJO_HOME") {
        assert!(!p.is_empty(), "DOJJO_HOME must not be empty when set");
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME must be set to locate ~/.dojjo")?;
    assert!(!home.as_os_str().is_empty(), "HOME must not be empty");
    Ok(home.join(".dojjo"))
}

pub fn dojo_home(dojo_id: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let base = dojjo_home_base()?;
    Ok(base.join("dojos").join(dojo_id))
}

/// Workspace root for the hidden `default` workspace (physical repo host).
pub fn default_workspace_root(dojo_home: &Path) -> PathBuf {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    dojo_home.join(DEFAULT_WORKSPACE_DIR)
}

pub fn dojo_config_path(dojo_home: &Path) -> PathBuf {
    dojo_home.join(CONFIG_FILE)
}

pub fn dojo_state_path(dojo_home: &Path) -> PathBuf {
    dojo_home.join(STATE_FILE)
}

pub fn repo_machine_config_path(dojo_home: &Path) -> PathBuf {
    dojo_home.join(REPO_MACHINE_FILE)
}

/// Resolve `.jj/repo` at `jj_dir` (the `.jj` directory): directory or pointer file (JJ convention).
pub fn resolve_repo_path_at_jj(jj_dir: &Path) -> anyhow::Result<PathBuf> {
    assert!(jj_dir.as_os_str().len() > 0, "jj_dir must not be empty");
    let repo_path = jj_dir.join("repo");
    assert!(
        repo_path.starts_with(jj_dir),
        "repo path must be under jj_dir"
    );

    if repo_path.is_dir() {
        return repo_path
            .canonicalize()
            .with_context(|| format!("canonicalize repo dir {}", repo_path.display()));
    }

    let meta = repo_path
        .symlink_metadata()
        .with_context(|| format!("stat {}", repo_path.display()))?;
    if !meta.is_file() {
        anyhow::bail!(
            ".jj/repo must be a directory or pointer file (got {:?})",
            repo_path.display()
        );
    }

    let raw = std::fs::read_to_string(&repo_path)
        .with_context(|| format!("read repo pointer {}", repo_path.display()))?;
    let rel = raw.trim();
    anyhow::ensure!(
        !rel.is_empty(),
        "repo pointer file must not be empty: {}",
        repo_path.display()
    );

    // JJ may store an absolute path when /var vs /private/var (macOS) prevents a relative one.
    let joined = if rel.starts_with('/') {
        PathBuf::from(rel)
    } else {
        jj_dir.join(rel)
    };
    let canon = joined
        .canonicalize()
        .with_context(|| format!("canonicalize repo via pointer {}", joined.display()))?;
    assert!(canon.is_dir(), "resolved repo path must be a directory");
    Ok(canon)
}

/// Canonical `.jj/repo` directory from a path inside a workspace (walks up for `.jj`).
pub fn find_jj_dir_from(start: &Path) -> anyhow::Result<PathBuf> {
    assert!(start.as_os_str().len() > 0, "start must not be non-empty");
    let mut cur = start
        .canonicalize()
        .with_context(|| format!("canonicalize {}", start.display()))?;
    loop {
        let jj_dir = cur.join(".jj");
        let repo_at = jj_dir.join("repo");
        if repo_at.exists() {
            assert!(jj_dir.is_dir(), ".jj must be a directory when repo exists");
            return Ok(jj_dir
                .canonicalize()
                .with_context(|| format!("canonicalize {}", jj_dir.display()))?);
        }
        if !cur.pop() {
            anyhow::bail!(
                "no .jj/repo found from {} (run from inside a Jujutsu workspace)",
                start.display()
            );
        }
    }
}

/// Workspace root (parent of `.jj`).
pub fn workspace_root_from_jj(jj_dir: &Path) -> anyhow::Result<PathBuf> {
    assert!(jj_dir.as_os_str().len() > 0, "jj_dir must not be empty");
    let root = jj_dir
        .parent()
        .context(".jj must have a parent workspace root")?;
    root.canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", root.display()))
}

/// Write relative path from `jj_dir/.jj/repo` pointer file to `canonical_repo`.
pub fn write_repo_pointer(jj_dir: &Path, canonical_repo: &Path) -> anyhow::Result<()> {
    assert!(jj_dir.as_os_str().len() > 0, "jj_dir must not be empty");
    assert!(
        canonical_repo.is_dir(),
        "canonical_repo must be an existing directory"
    );
    let canon_repo = canonical_repo
        .canonicalize()
        .with_context(|| format!("canonicalize {}", canonical_repo.display()))?;
    let canon_jj = jj_dir
        .canonicalize()
        .with_context(|| format!("canonicalize {}", jj_dir.display()))?;
    assert!(
        canon_repo != canon_jj,
        "repo pointer must target a different path than .jj itself"
    );

    let rel = pathdiff::diff_paths(&canon_repo, &canon_jj)
        .with_context(|| format!("relative path from {} to {}", canon_jj.display(), canon_repo.display()))?;
    let rel = rel.to_str().context("repo pointer path must be UTF-8")?;
    anyhow::ensure!(!rel.is_empty(), "repo pointer path must not be empty");

    let pointer = canon_jj.join("repo");
    std::fs::write(&pointer, rel.as_bytes())
        .with_context(|| format!("write repo pointer {}", pointer.display()))?;
    Ok(())
}

/// Physical repo used by dojjo sync: `_default/.jj/repo` (directory, not a pointer).
pub fn sync_repo_root(dojo_home: &Path) -> anyhow::Result<PathBuf> {
    let jj_dir = default_workspace_root(dojo_home).join(".jj");
    if !jj_dir.join("repo").exists() {
        anyhow::bail!(
            "dojo sync repo not initialized at {} (run dojjo create or join first)",
            jj_dir.display()
        );
    }
    let canon = resolve_repo_path_at_jj(&jj_dir)?;
    let pointer_path = jj_dir.join("repo");
    assert!(
        pointer_path.is_dir(),
        "sync host .jj/repo must be a directory, not a pointer (got {})",
        pointer_path.display()
    );
    Ok(canon)
}

pub fn load_config(dojo_home: &Path) -> anyhow::Result<DojoConfig> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let path = dojo_config_path(dojo_home);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    assert!(!bytes.is_empty(), "config file must not be empty");
    let c: DojoConfig = serde_json::from_slice(&bytes).context("parse dojo config")?;
    assert!(!c.dojo_id.is_empty(), "dojo_id in config must not be empty");
    assert!(!c.api_base.is_empty(), "api_base in config must not be empty");
    Ok(c)
}

pub fn save_config(dojo_home: &Path, cfg: &DojoConfig) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    std::fs::create_dir_all(dojo_home)
        .with_context(|| format!("create_dir_all {}", dojo_home.display()))?;
    let path = dojo_config_path(dojo_home);
    let json = serde_json::to_vec_pretty(cfg).context("serialize config")?;
    atomic_write(&path, &json).context("write dojo config")?;
    Ok(())
}

pub fn load_state(dojo_home: &Path) -> anyhow::Result<DojoState> {
    let path = dojo_state_path(dojo_home);
    if !path.exists() {
        return Ok(DojoState::default());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    assert!(!bytes.is_empty(), "state file must not be empty when present");
    let s: DojoState = serde_json::from_slice(&bytes).context("parse dojo state")?;
    Ok(s)
}

pub fn save_state(dojo_home: &Path, state: &DojoState) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    std::fs::create_dir_all(dojo_home)
        .with_context(|| format!("create_dir_all {}", dojo_home.display()))?;
    let path = dojo_state_path(dojo_home);
    let json = serde_json::to_vec_pretty(state).context("serialize state")?;
    atomic_write(&path, &json).context("write dojo state")?;
    Ok(())
}

pub fn load_repo_machine_config(dojo_home: &Path) -> anyhow::Result<RepoMachineConfig> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let path = repo_machine_config_path(dojo_home);
    if !path.exists() {
        return Ok(RepoMachineConfig::default());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    assert!(
        !bytes.is_empty(),
        "repo machine config file must not be empty when present"
    );
    let cfg: RepoMachineConfig =
        serde_json::from_slice(&bytes).context("parse repo machine config")?;
    Ok(cfg)
}

pub fn save_repo_machine_config(dojo_home: &Path, cfg: &RepoMachineConfig) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    std::fs::create_dir_all(dojo_home)
        .with_context(|| format!("create_dir_all {}", dojo_home.display()))?;
    let path = repo_machine_config_path(dojo_home);
    let json = serde_json::to_vec_pretty(cfg).context("serialize repo machine config")?;
    atomic_write(&path, &json).context("write repo machine config")?;
    Ok(())
}

pub fn load_or_create_repo_machine_config(dojo_home: &Path) -> anyhow::Result<RepoMachineConfig> {
    let cfg = load_repo_machine_config(dojo_home)?;
    let path = repo_machine_config_path(dojo_home);
    if !path.exists() {
        save_repo_machine_config(dojo_home, &cfg)?;
    }
    Ok(cfg)
}

pub fn write_workspace_link(jj_dir: &Path, dojo_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!dojo_id.is_empty(), "dojo_id must not be empty");
    let link = DojoLink {
        dojo_id: dojo_id.to_string(),
    };
    let path = jj_dir.join(DOJJO_LINK_FILE);
    let json = serde_json::to_vec_pretty(&link).context("serialize dojo link")?;
    atomic_write(&path, &json).context("write workspace link")?;
    Ok(())
}

pub fn read_workspace_link(jj_dir: &Path) -> anyhow::Result<Option<DojoLink>> {
    let path = jj_dir.join(DOJJO_LINK_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let link: DojoLink = serde_json::from_slice(&bytes).context("parse dojo link")?;
    assert!(!link.dojo_id.is_empty(), "dojo_id in link must not be empty");
    Ok(Some(link))
}

/// Locate dojo metadata for a workspace by `.jj/dojjo.json` or canonical repo match.
pub fn find_dojo_home_for_workspace(start: &Path) -> anyhow::Result<PathBuf> {
    let jj_dir = find_jj_dir_from(start)?;
    if let Some(link) = read_workspace_link(&jj_dir)? {
        let home = dojo_home(&link.dojo_id)?;
        if dojo_config_path(&home).is_file() {
            if workspace_rehomed_to_dojo(&jj_dir, &home)? {
                return Ok(home);
            }
            anyhow::bail!(
                "workspace {} is linked to dojo {} but its .jj/repo is not tracked by DOJJO_HOME={} (expected sync repo under {})",
                start.display(),
                link.dojo_id,
                dojjo_home_base()?.display(),
                home.display()
            );
        }
    }

    let canon = resolve_repo_path_at_jj(&jj_dir)?;
    find_dojo_home_for_canonical_repo(&canon)
}

fn find_dojo_home_for_canonical_repo(canon_repo: &Path) -> anyhow::Result<PathBuf> {
    let base = dojjo_home_base()?;
    let dojos_dir = base.join("dojos");
    if !dojos_dir.is_dir() {
        anyhow::bail!("no dojo registry at {}", dojos_dir.display());
    }

    let target = canon_repo
        .canonicalize()
        .with_context(|| format!("canonicalize {}", canon_repo.display()))?;

    for ent in std::fs::read_dir(&dojos_dir).with_context(|| format!("read_dir {}", dojos_dir.display()))? {
        let ent = ent?;
        if !ent.file_type().context("file_type")?.is_dir() {
            continue;
        }
        let home = ent.path();
        let sync_repo = sync_repo_root(&home);
        if sync_repo.is_err() {
            continue;
        }
        let sync_repo = sync_repo.expect("sync_repo_root ok");
        if sync_repo == target {
            return Ok(home);
        }
    }

    anyhow::bail!(
        "no dojo owns repo {}; run dojjo create or join from this workspace",
        target.display()
    )
}

/// Rewrite `.jj/repo` pointers for every workspace root except `user_workspace_root`.
pub fn repoint_sibling_workspaces(
    workspace_roots: &[(String, PathBuf)],
    user_workspace_root: &Path,
    default_host_root: &Path,
    new_repo: &Path,
) -> anyhow::Result<()> {
    assert!(new_repo.is_dir(), "new_repo must be a directory");
    let user_workspace_root = user_workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", user_workspace_root.display()))?;
    let default_host_root = default_host_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", default_host_root.display()))?;
    let new_repo = new_repo
        .canonicalize()
        .with_context(|| format!("canonicalize {}", new_repo.display()))?;

    for (name, root) in workspace_roots {
        assert!(!name.is_empty(), "workspace name must not be empty");
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalize workspace root {}", root.display()))?;
        if root == user_workspace_root {
            continue;
        }
        if root.starts_with(&default_host_root) {
            continue;
        }
        assert!(root.is_dir(), "sibling workspace root must be a directory");
        let jj_dir = root.join(".jj");
        assert!(jj_dir.is_dir(), "sibling workspace must have .jj/");
        write_repo_pointer(&jj_dir, &new_repo)
            .with_context(|| format!("repoint workspace {name} at {}", root.display()))?;
        let link = jj_dir.join(DOJJO_LINK_FILE);
        if link.is_file() {
            fs::remove_file(&link)
                .with_context(|| format!("remove {}", link.display()))?;
        }
    }
    Ok(())
}

/// True when `path` is under `prefix` (both should be canonical).
pub fn path_is_under(path: &Path, prefix: &Path) -> bool {
    assert!(path.as_os_str().len() > 0, "path must not be empty");
    assert!(prefix.as_os_str().len() > 0, "prefix must not be empty");
    path.starts_with(prefix)
}

/// Sanitized hostname for use as a JJ workspace name.
pub fn default_user_workspace_name() -> anyhow::Result<String> {
    let host = hostname::get().context("hostname")?;
    let host = host.to_str().context("hostname must be UTF-8")?;
    let mut out = String::new();
    for c in host.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c.to_ascii_lowercase());
        } else if c == '.' || c == '_' {
            out.push('-');
        }
    }
    anyhow::ensure!(!out.is_empty(), "hostname produced empty workspace name");
    anyhow::ensure!(
        out != DEFAULT_WORKSPACE_NAME,
        "hostname must not equal reserved workspace name {DEFAULT_WORKSPACE_NAME}"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct DojjoHomeRestore(Option<std::ffi::OsString>);

    impl Drop for DojjoHomeRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => unsafe { std::env::set_var("DOJJO_HOME", v) },
                None => unsafe { std::env::remove_var("DOJJO_HOME") },
            }
        }
    }

    #[test]
    fn atomic_write_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        atomic_write(&path, br#"{"api_base":"http://localhost:3000/api"}"#).unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got.contains("api_base"));
        assert!(!dir.path().join("config.json.tmp").exists());
    }

    #[test]
    fn path_is_under_prefix() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();
        let parent = parent.canonicalize().unwrap();
        let child = child.canonicalize().unwrap();
        assert!(path_is_under(&child, &parent));
        assert!(!path_is_under(&parent, &child));
    }

    #[test]
    fn resolve_repo_pointer() {
        let dir = tempdir().unwrap();
        let host = dir.path().join("host");
        let ws = dir.path().join("ws");
        fs::create_dir_all(host.join("store")).unwrap();
        fs::create_dir_all(ws.join(".jj")).unwrap();
        write_repo_pointer(&ws.join(".jj"), &host).unwrap();
        let got = resolve_repo_path_at_jj(&ws.join(".jj")).unwrap();
        assert_eq!(got, host.canonicalize().unwrap());
    }

    #[test]
    fn resolve_repo_pointer_absolute() {
        let dir = tempdir().unwrap();
        let host = dir.path().join("host");
        let ws = dir.path().join("ws");
        fs::create_dir_all(host.join("store")).unwrap();
        fs::create_dir_all(ws.join(".jj")).unwrap();
        let abs = host.canonicalize().unwrap();
        fs::write(ws.join(".jj").join("repo"), abs.to_str().unwrap()).unwrap();
        let got = resolve_repo_path_at_jj(&ws.join(".jj")).unwrap();
        assert_eq!(got, abs);
    }

    #[test]
    fn linked_workspace_must_match_current_dojjo_home_repo() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempdir().unwrap();
        let dojjo_home = dir.path().join("dojjo-home");
        let dojo_home = dojjo_home.join("dojos").join("d1");
        let ws = dir.path().join("workspace");
        let ws_jj = ws.join(".jj");
        let unrelated_repo = dir.path().join("unrelated-repo");

        fs::create_dir_all(&ws_jj).unwrap();
        fs::create_dir_all(unrelated_repo.join("store")).unwrap();
        fs::create_dir_all(dojo_home.join("_default").join(".jj").join("repo").join("store"))
            .unwrap();

        let cfg = DojoConfig {
            dojo_id: "d1".to_string(),
            api_base: "http://localhost:3000/api".to_string(),
            git_remote_url: None,
        };
        save_config(&dojo_home, &cfg).unwrap();
        write_workspace_link(&ws_jj, "d1").unwrap();
        write_repo_pointer(&ws_jj, &unrelated_repo).unwrap();

        let _restore = DojjoHomeRestore(std::env::var_os("DOJJO_HOME"));
        unsafe {
            std::env::set_var("DOJJO_HOME", &dojjo_home);
        }
        let err = find_dojo_home_for_workspace(&ws).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not tracked by DOJJO_HOME"),
            "unexpected error: {msg}"
        );
    }
}
