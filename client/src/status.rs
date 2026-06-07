//! `dojjo status` — local and server diagnostics (always exits 0).

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use crate::background_sync;
use crate::config::{self, DojoLink, DojoState, DOJJO_LINK_FILE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Line {
    level: Level,
    label: String,
    detail: Option<String>,
}

impl Line {
    fn ok(label: impl Into<String>) -> Self {
        Self {
            level: Level::Ok,
            label: label.into(),
            detail: None,
        }
    }

    fn ok_detail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level: Level::Ok,
            label: label.into(),
            detail: Some(detail.into()),
        }
    }

    fn warn(label: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            level: Level::Warn,
            label: label.into(),
            detail,
        }
    }

    fn fail(label: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            level: Level::Fail,
            label: label.into(),
            detail,
        }
    }
}

fn level_tag(level: Level) -> &'static str {
    match level {
        Level::Ok => "ok  ",
        Level::Warn => "warn",
        Level::Fail => "fail",
    }
}

fn print_section(title: &str, lines: &[Line]) {
    assert!(!title.is_empty(), "section title must not be empty");
    println!();
    println!("{title}");
    for line in lines {
        if let Some(d) = &line.detail {
            println!("  {}  {} ({})", level_tag(line.level), line.label, d);
        } else {
            println!("  {}  {}", level_tag(line.level), line.label);
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct HealthResponse {
    ok: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ServerDojoStatus {
    registered_in_db: bool,
    data_dir_exists: bool,
    bare_git_exists: bool,
    bare_receivepack_enabled: Option<bool>,
    mirror_dir_exists: bool,
    mirror_has_files: bool,
    public_url_configured: bool,
    git_remote_mode: String,
    git_remote_url: Option<String>,
}

struct WorkspaceCtx {
    workspace_root: PathBuf,
    jj_dir: PathBuf,
    link: Option<DojoLink>,
    canon_repo: PathBuf,
    dojo_home: Option<PathBuf>,
}

fn try_workspace_ctx(cwd: &Path) -> Option<WorkspaceCtx> {
    let jj_dir = config::find_jj_dir_from(cwd).ok()?;
    let workspace_root = config::workspace_root_from_jj(&jj_dir).ok()?;
    let link = config::read_workspace_link(&jj_dir).ok().flatten();
    let canon_repo = config::resolve_repo_path_at_jj(&jj_dir).ok()?;
    let dojo_home = try_dojo_home(&jj_dir, &link, &canon_repo);
    Some(WorkspaceCtx {
        workspace_root,
        jj_dir,
        link,
        canon_repo,
        dojo_home,
    })
}

fn try_dojo_home(_jj_dir: &Path, link: &Option<DojoLink>, canon_repo: &Path) -> Option<PathBuf> {
    if let Some(link) = link {
        let home = config::dojo_home(&link.dojo_id).ok()?;
        if config::dojo_config_path(&home).is_file() {
            return Some(home);
        }
    }
    try_dojo_home_for_canonical_repo(canon_repo)
}

fn try_dojo_home_for_canonical_repo(canon_repo: &Path) -> Option<PathBuf> {
    let base = config::dojjo_home_base().ok()?;
    let dojos_dir = base.join("dojos");
    if !dojos_dir.is_dir() {
        return None;
    }
    let target = canon_repo.canonicalize().ok()?;
    let entries = std::fs::read_dir(&dojos_dir).ok()?;
    for ent in entries.flatten() {
        if !ent.file_type().ok()?.is_dir() {
            continue;
        }
        let home = ent.path();
        let sync_repo = config::sync_repo_root(&home).ok()?;
        if sync_repo == target {
            return Some(home);
        }
    }
    None
}

fn global_lines() -> Vec<Line> {
    let mut lines = Vec::new();
    let base = match config::dojjo_home_base() {
        Ok(b) => b,
        Err(e) => {
            lines.push(Line::fail("DOJJO_HOME / HOME", Some(e.to_string())));
            return lines;
        }
    };
    lines.push(Line::ok_detail("dojjo home", base.display().to_string()));

    let global_path = base.join(config::GLOBAL_CONFIG_FILE);
    match config::load_global_config() {
        Ok(cfg) => {
            lines.push(Line::ok_detail(
                "global config",
                global_path.display().to_string(),
            ));
            lines.push(Line::ok_detail("api_base", cfg.api_base));
        }
        Err(e) => {
            lines.push(Line::fail(
                "global config",
                Some(format!("{} ({})", global_path.display(), e)),
            ));
            lines.push(Line::warn(
                "run `dojjo init` to configure the sync-server API URL",
                None,
            ));
        }
    }
    lines
}

fn workspace_lines(ctx: &WorkspaceCtx) -> Vec<Line> {
    let mut lines = Vec::new();
    lines.push(Line::ok_detail(
        "workspace root",
        ctx.workspace_root.display().to_string(),
    ));
    lines.push(Line::ok_detail(".jj directory", ctx.jj_dir.display().to_string()));

    let link_path = ctx.jj_dir.join(DOJJO_LINK_FILE);
    match &ctx.link {
        Some(link) => lines.push(Line::ok_detail(
            format!("{DOJJO_LINK_FILE}"),
            format!("dojo_id={}", link.dojo_id),
        )),
        None => lines.push(Line::warn(
            format!("no {DOJJO_LINK_FILE} (workspace not explicitly linked)"),
            Some(link_path.display().to_string()),
        )),
    }

    let repo_meta = ctx.jj_dir.join("repo");
    if repo_meta.is_dir() {
        lines.push(Line::warn(
            ".jj/repo is a directory (not re-homed to a dojo sync host)",
            Some(ctx.canon_repo.display().to_string()),
        ));
    } else if repo_meta.is_file() {
        let rehomed = ctx
            .dojo_home
            .as_ref()
            .and_then(|home| config::workspace_rehomed_to_dojo(&ctx.jj_dir, home).ok())
            .unwrap_or(false);
        if rehomed {
            lines.push(Line::ok_detail(
                ".jj/repo pointer targets dojo sync host",
                ctx.canon_repo.display().to_string(),
            ));
        } else {
            lines.push(Line::warn(
                ".jj/repo pointer does not target this dojo's sync host",
                Some(ctx.canon_repo.display().to_string()),
            ));
        }
    } else {
        lines.push(Line::fail(".jj/repo missing", None));
    }

    match &ctx.dojo_home {
        Some(h) => lines.push(Line::ok_detail(
            "local dojo home",
            h.display().to_string(),
        )),
        None => lines.push(Line::warn(
            "no local dojo home for this workspace",
            Some("run dojjo create or dojjo join".into()),
        )),
    }
    lines
}

fn local_dojo_lines(dojo_home: &Path) -> Vec<Line> {
    let mut lines = Vec::new();
    let complete = config::is_dojo_create_complete(dojo_home);
    if complete {
        lines.push(Line::ok("create finished (config.json present)"));
    } else if let Ok(Some(progress)) = config::load_create_progress(dojo_home) {
        lines.push(Line::warn(
            "create in progress",
            Some(format!("phase {:?}", progress.phase)),
        ));
    } else {
        lines.push(Line::warn("create not finished", None));
    }

    let mut local_git_remote: Option<String> = None;

    match config::load_config(dojo_home) {
        Ok(cfg) => {
            local_git_remote = cfg.git_remote_url.clone();
            lines.push(Line::ok_detail("dojo_id", cfg.dojo_id));
            lines.push(Line::ok_detail("api_base (dojo)", cfg.api_base));
            match &cfg.git_remote_url {
                Some(u) if !u.is_empty() => {
                    lines.push(Line::ok_detail("git_remote_url (local config)", u.clone()))
                }
                _ => lines.push(Line::warn(
                    "git_remote_url missing in local config",
                    Some("re-run dojjo join or create with a current sync-server".into()),
                )),
            }
        }
        Err(e) => {
            lines.push(Line::fail(
                "dojo config.json",
                Some(format!("{} ({})", config::dojo_config_path(dojo_home).display(), e)),
            ));
        }
    }

    match config::sync_repo_root(dojo_home) {
        Ok(sync) => lines.push(Line::ok_detail(
            "sync host repo (_default/.jj/repo)",
            sync.display().to_string(),
        )),
        Err(e) => lines.push(Line::warn(
            "sync host repo not initialized",
            Some(e.to_string()),
        )),
    }

    let state: DojoState = config::load_state(dojo_home).unwrap_or_default();
    if state.last_remote_revision.is_empty() {
        lines.push(Line::warn("no last_remote_revision recorded locally", None));
    } else {
        lines.push(Line::ok_detail(
            "last_remote_revision",
            state.last_remote_revision.clone(),
        ));
    }

    if let Ok(Some(progress)) = config::load_create_progress(dojo_home) {
        let _ = progress;
        lines.push(Line::ok_detail(
            "create_progress.json",
            config::dojo_create_progress_path(dojo_home).display().to_string(),
        ));
    }

    match background_sync::status_for_dojo_home(dojo_home) {
        Ok((cfg, running)) => {
            if cfg.background_sync_enabled {
                lines.push(Line::ok(
                    "background sync enabled on this machine for this dojo",
                ));
            } else {
                lines.push(Line::warn(
                    "background sync disabled on this machine for this dojo",
                    Some("run `dojjo background-sync enable` to turn it back on".into()),
                ));
            }
            if running {
                lines.push(Line::ok("background sync worker process is running"));
            } else if cfg.background_sync_enabled {
                lines.push(Line::warn(
                    "background sync worker process is not running",
                    Some("run `dojjo background-sync enable` to restart it".into()),
                ));
            } else {
                lines.push(Line::ok("background sync worker is stopped (expected: disabled)"));
            }
        }
        Err(e) => lines.push(Line::warn(
            "unable to read background sync state",
            Some(e.to_string()),
        )),
    }

    lines.extend(git_remote_probe_lines(local_git_remote.as_deref()));
    lines
}

fn git_remote_probe_lines(url: Option<&str>) -> Vec<Line> {
    let Some(url) = url else {
        return Vec::new();
    };
    assert!(!url.is_empty(), "url must not be empty when Some");
    let mut lines = Vec::new();
    if url.starts_with("file://") {
        let path = url.strip_prefix("file://").unwrap_or(url);
        if Path::new(path).is_dir() {
            lines.push(Line::ok_detail("git remote path reachable locally", path.to_string()));
        } else {
            lines.push(Line::fail(
                "git remote path not found on this machine",
                Some(path.to_string()),
            ));
        }
        return lines;
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let out = std::process::Command::new("git")
            .args(["ls-remote", "--heads", url])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                lines.push(Line::ok("git ls-remote (HTTP remote reachable)"));
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let detail = if err.trim().is_empty() {
                    format!("exit {}", o.status.code().unwrap_or(-1))
                } else {
                    err.trim().to_string()
                };
                lines.push(Line::fail("git ls-remote failed", Some(detail)));
            }
            Err(e) => lines.push(Line::fail("git ls-remote spawn failed", Some(e.to_string()))),
        }
        return lines;
    }
    lines.push(Line::warn(
        "unsupported git_remote_url scheme for probe",
        Some(url.to_string()),
    ));
    lines
}

async fn server_section(
    api_base: &str,
    dojo_id: Option<&str>,
    local_git_remote: Option<&str>,
) -> Vec<Line> {
    let client = match Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return vec![Line::fail(
                "HTTP client setup failed",
                Some(e.to_string()),
            )];
        }
    };
    let Some(dojo_id) = dojo_id else {
        return server_lines_health_only(&client, api_base).await;
    };
    server_lines(&client, api_base, dojo_id, local_git_remote).await
}

async fn server_lines_health_only(client: &Client, api_base: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let base = api_base.trim_end_matches('/');
    let health_url = format!("{base}/health");
    match client.get(&health_url).send().await {
        Ok(res) if res.status().is_success() => {
            lines.push(Line::ok_detail("API reachable", health_url));
        }
        Ok(res) => lines.push(Line::fail(
            "API health check failed",
            Some(format!("HTTP {}", res.status())),
        )),
        Err(e) => lines.push(Line::fail(
            "cannot reach sync-server API",
            Some(format!("{health_url}: {e}")),
        )),
    }
    lines
}

async fn server_lines(
    client: &Client,
    api_base: &str,
    dojo_id: &str,
    local_git_remote: Option<&str>,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let base = api_base.trim_end_matches('/');
    assert!(!base.is_empty(), "api_base must not be empty");

    let health_url = format!("{base}/health");
    match client.get(&health_url).send().await {
        Ok(res) if res.status().is_success() => {
            let parsed: Result<HealthResponse, _> = res.json().await;
            match parsed {
                Ok(h) if h.ok => lines.push(Line::ok_detail("API reachable", health_url)),
                Ok(_) => lines.push(Line::warn("API health returned ok=false", Some(health_url))),
                Err(e) => lines.push(Line::warn(
                    "API reachable but health JSON unexpected",
                    Some(e.to_string()),
                )),
            }
        }
        Ok(res) => lines.push(Line::fail(
            "API health check failed",
            Some(format!("{} HTTP {}", health_url, res.status())),
        )),
        Err(e) => lines.push(Line::fail(
            "cannot reach sync-server API",
            Some(format!("{health_url}: {e}")),
        )),
    }

    let dojo_url = format!("{base}/dojo/{dojo_id}");
    match client.get(&dojo_url).send().await {
        Ok(res) if res.status().is_success() => {
            lines.push(Line::ok_detail("GET /dojo/{{id}}", dojo_id.to_string()));
        }
        Ok(res) if res.status() == reqwest::StatusCode::NOT_FOUND => {
            lines.push(Line::fail(
                "dojo not found on server",
                Some(format!("check dojo_id and api_base ({dojo_url})")),
            ));
        }
        Ok(res) => lines.push(Line::fail(
            "GET /dojo/{{id}} failed",
            Some(format!("HTTP {}", res.status())),
        )),
        Err(e) => lines.push(Line::fail("GET /dojo/{{id}} request failed", Some(e.to_string()))),
    }

    let status_url = format!("{base}/dojo/{dojo_id}/status");
    let status_res = client.get(&status_url).send().await;
    let parsed: Option<ServerDojoStatus> = match status_res {
        Ok(res) if res.status().is_success() => match res.json().await {
            Ok(s) => Some(s),
            Err(e) => {
                lines.push(Line::fail(
                    "GET /dojo/{{id}}/status JSON parse failed",
                    Some(e.to_string()),
                ));
                None
            }
        },
        Ok(res) if res.status() == reqwest::StatusCode::NOT_FOUND => {
            lines.push(Line::fail("dojo status not found on server", Some(dojo_id.to_string())));
            None
        }
        Ok(res) => {
            lines.push(Line::fail(
                "GET /dojo/{{id}}/status failed",
                Some(format!("HTTP {}", res.status())),
            ));
            None
        }
        Err(e) => {
            lines.push(Line::fail(
                "GET /dojo/{{id}}/status request failed",
                Some(e.to_string()),
            ));
            None
        }
    };

    let Some(st) = parsed else {
        return lines;
    };

    push_server_flag(&mut lines, "registered in database", st.registered_in_db);
    push_server_flag(&mut lines, "dojo data directory", st.data_dir_exists);
    push_server_flag(&mut lines, "bare git repository", st.bare_git_exists);
    match st.bare_receivepack_enabled {
        Some(true) => lines.push(Line::ok_detail(
            "HTTP git push allowed",
            "receive-pack via git http-backend (REMOTE_USER set on server)",
        )),
        Some(false) => lines.push(Line::fail(
            "HTTP git push disabled",
            Some("bare repo has http.receivepack=false".into()),
        )),
        None => lines.push(Line::warn("HTTP git push unknown", None)),
    }
    push_server_flag(&mut lines, "mirror/ directory", st.mirror_dir_exists);
    if st.mirror_dir_exists && !st.mirror_has_files {
        lines.push(Line::warn(
            "mirror/ has no files yet",
            Some("upload or sync JJ mirror blobs to the server".into()),
        ));
    } else if st.mirror_has_files {
        lines.push(Line::ok("mirror/ contains uploaded files"));
    }
    if st.public_url_configured {
        lines.push(Line::ok_detail(
            "server git remote mode",
            format!("{} (DOJJO_PUBLIC_URL set)", st.git_remote_mode),
        ));
    } else {
        lines.push(Line::warn(
            "server git remote mode",
            Some(format!(
                "{} (DOJJO_PUBLIC_URL unset — remotes are file:// on the server host)",
                st.git_remote_mode
            )),
        ));
    }

    if let Some(server_url) = &st.git_remote_url {
        lines.push(Line::ok_detail("git_remote_url (server)", server_url.clone()));
        match local_git_remote {
            Some(local) if local == server_url => {
                lines.push(Line::ok("local and server git_remote_url match"));
            }
            Some(local) => lines.push(Line::fail(
                "local and server git_remote_url differ",
                Some(format!("local={local:?} server={server_url:?}")),
            )),
            None => lines.push(Line::warn(
                "cannot compare git_remote_url (missing locally)",
                None,
            )),
        }
    } else if st.bare_git_exists {
        lines.push(Line::fail("server did not return git_remote_url", None));
    }

    let manifest_url = format!("{base}/dojo/{dojo_id}/mirror/manifest");
    match client.get(&manifest_url).send().await {
        Ok(res) if res.status().is_success() => {
            if let Ok(body) = res.json::<serde_json::Value>().await {
                let rev = body.get("revision").and_then(|v| v.as_str()).unwrap_or("");
                if rev.is_empty() {
                    lines.push(Line::warn("mirror manifest has empty revision", None));
                } else {
                    let n = body
                        .get("entries")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    lines.push(Line::ok_detail(
                        "mirror manifest",
                        format!("revision {rev}, {n} entries"),
                    ));
                }
            } else {
                lines.push(Line::warn("mirror manifest JSON unreadable", None));
            }
        }
        Ok(res) if res.status() == reqwest::StatusCode::NOT_FOUND => {
            lines.push(Line::warn("mirror manifest not found", None));
        }
        Ok(res) => lines.push(Line::warn(
            "mirror manifest request failed",
            Some(format!("HTTP {}", res.status())),
        )),
        Err(e) => lines.push(Line::warn(
            "mirror manifest request error",
            Some(e.to_string()),
        )),
    }

    lines
}

fn push_server_flag(lines: &mut Vec<Line>, label: &str, ok: bool) {
    assert!(!label.is_empty(), "label must not be empty");
    if ok {
        lines.push(Line::ok(label));
    } else {
        lines.push(Line::fail(label, None));
    }
}

pub async fn run(cwd: &Path) -> anyhow::Result<()> {
    assert!(cwd.as_os_str().len() > 0, "cwd must not be empty");
    println!("dojjo status");
    println!("cwd {}", cwd.display());

    print_section("Global", &global_lines());

    let api_base = config::resolve_api_base(None).ok();

    let ws = try_workspace_ctx(cwd);
    if let Some(ref ctx) = ws {
        print_section("Workspace", &workspace_lines(ctx));

        let dojo_id = ctx
            .dojo_home
            .as_ref()
            .and_then(|home| config::load_config(home).ok().map(|c| c.dojo_id))
            .or_else(|| ctx.link.as_ref().map(|l| l.dojo_id.clone()));

        if let Some(ref home) = ctx.dojo_home {
            let local = local_dojo_lines(home);
            print_section(
                &format!(
                    "Local dojo ({})",
                    dojo_id.as_deref().unwrap_or("unknown")
                ),
                &local,
            );
        } else if let Some(ref id) = dojo_id {
            print_section(
                "Local dojo",
                &[Line::warn(
                    "no local dojo home",
                    Some(format!("linked dojo_id={id} — run dojjo create or join")),
                )],
            );
        }

        let local_git = ctx
            .dojo_home
            .as_ref()
            .and_then(|home| config::load_config(home).ok())
            .and_then(|c| c.git_remote_url);
        let api_for_dojo = ctx
            .dojo_home
            .as_ref()
            .and_then(|home| config::load_config(home).ok())
            .map(|c| config::normalize_api_base(&c.api_base));

        if let (Some(base), Some(id)) = (api_for_dojo.or(api_base.clone()), dojo_id) {
            print_section("Server", &server_section(&base, Some(&id), local_git.as_deref()).await);
        } else if api_base.is_some() {
            let base = api_base.clone().expect("api_base checked");
            let mut server = server_section(&base, None, None).await;
            server.insert(
                0,
                Line::warn(
                    "no dojo_id — skipping dojo-specific server checks",
                    None,
                ),
            );
            print_section("Server", &server);
        } else {
            print_section(
                "Server",
                &[Line::warn(
                    "no api_base configured — skipping server checks",
                    Some("run dojjo init".into()),
                )],
            );
        }
    } else {
        print_section(
            "Workspace",
            &[Line::warn(
                "not inside a Jujutsu workspace",
                Some("run from a project directory with .jj/repo".into()),
            )],
        );
        if let Some(base) = api_base {
            let mut lines = server_section(&base, None, None).await;
            lines.push(Line::warn(
                "not in a linked dojo workspace — no dojo-specific checks",
                None,
            ));
            print_section("Server", &lines);
        }
    }

    Ok(())
}
