use std::path::{Path, PathBuf};
use std::fs::OpenOptions;
use std::process::Command;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::Client;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::config::{self, RepoMachineConfig};
use crate::sync;

const BACKGROUND_SYNC_INTERVAL_SECS: u64 = 10;
const BACKGROUND_SYNC_PID_FILE: &str = "background_sync.pid";
const BACKGROUND_SYNC_LOG_FILE: &str = "background_sync.log";

fn pid_file(dojo_home: &Path) -> PathBuf {
    dojo_home.join(BACKGROUND_SYNC_PID_FILE)
}

fn log_file(dojo_home: &Path) -> PathBuf {
    dojo_home.join(BACKGROUND_SYNC_LOG_FILE)
}

fn parse_pid(raw: &str) -> Option<i32> {
    raw.trim().parse::<i32>().ok()
}

fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0) probes process existence on Unix.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn read_pid(dojo_home: &Path) -> anyhow::Result<Option<i32>> {
    let path = pid_file(dojo_home);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse_pid(&raw))
}

fn write_pid(dojo_home: &Path, pid: i32) -> anyhow::Result<()> {
    assert!(pid > 0, "pid must be positive");
    let path = pid_file(dojo_home);
    std::fs::write(&path, format!("{pid}\n")).with_context(|| format!("write {}", path.display()))
}

fn clear_pid(dojo_home: &Path) -> anyhow::Result<()> {
    let path = pid_file(dojo_home);
    if path.is_file() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

fn kill_pid(pid: i32) {
    if pid <= 0 {
        return;
    }
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

struct OperationsWatcher {
    _watcher: RecommendedWatcher,
    rx: UnboundedReceiver<notify::Result<Event>>,
}

fn operations_dir(jj_repo_folder: &Path) -> PathBuf {
    assert!(jj_repo_folder.as_os_str().len() > 0, "jj_repo_folder must not be empty");
    jj_repo_folder.join("op_store").join("operations")
}

fn create_operations_watcher(jj_repo_folder: &Path) -> anyhow::Result<Option<OperationsWatcher>> {
    assert!(jj_repo_folder.is_dir(), "jj_repo_folder must be a directory");
    let dir = operations_dir(jj_repo_folder);
    if !dir.is_dir() {
        return Ok(None);
    }
    let (tx, rx) = unbounded_channel();
    let mut watcher =
        notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .context("create operations watcher")?;
    watcher
        .watch(&dir, RecursiveMode::Recursive)
        .with_context(|| format!("watch {}", dir.display()))?;
    Ok(Some(OperationsWatcher {
        _watcher: watcher,
        rx,
    }))
}

fn drain_operations_events(watcher: &mut OperationsWatcher) -> bool {
    let mut saw_change = false;
    loop {
        match watcher.rx.try_recv() {
            Ok(Ok(_event)) => {
                saw_change = true;
            }
            Ok(Err(_watch_err)) => {
                // Keep running with polling even if a watch event parse fails.
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return saw_change,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return saw_change,
        }
    }
}

pub fn set_enabled_for_workspace(cwd: &Path, enabled: bool) -> anyhow::Result<PathBuf> {
    let dojo_home = config::find_dojo_home_for_workspace(cwd)?;
    let cfg = RepoMachineConfig {
        background_sync_enabled: enabled,
    };
    config::save_repo_machine_config(&dojo_home, &cfg)?;
    Ok(dojo_home)
}

pub fn status_for_workspace(cwd: &Path) -> anyhow::Result<(PathBuf, RepoMachineConfig, bool)> {
    let dojo_home = config::find_dojo_home_for_workspace(cwd)?;
    status_for_dojo_home(&dojo_home).map(|(cfg, running)| (dojo_home, cfg, running))
}

pub fn status_for_dojo_home(dojo_home: &Path) -> anyhow::Result<(RepoMachineConfig, bool)> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let cfg = config::load_or_create_repo_machine_config(&dojo_home)?;
    let running = read_pid(&dojo_home)?.is_some_and(process_is_alive);
    Ok((cfg, running))
}

pub fn stop_worker(dojo_home: &Path) -> anyhow::Result<()> {
    if let Some(pid) = read_pid(dojo_home)? {
        if process_is_alive(pid) {
            kill_pid(pid);
        }
    }
    clear_pid(dojo_home)?;
    Ok(())
}

pub fn ensure_worker_running(dojo_home: &Path) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let cfg = config::load_or_create_repo_machine_config(dojo_home)?;
    if !cfg.background_sync_enabled {
        return Ok(());
    }
    if let Some(pid) = read_pid(dojo_home)? {
        if process_is_alive(pid) {
            return Ok(());
        }
        clear_pid(dojo_home)?;
    }

    let exe = std::env::current_exe().context("resolve current executable")?;
    let log_path = log_file(dojo_home);
    let stdout_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let stderr_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let child = Command::new(&exe)
        .arg("dev")
        .arg("sync-worker")
        .arg("--dojo-home")
        .arg(dojo_home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .with_context(|| format!("spawn background sync worker via {}", exe.display()))?;
    let pid = child.id() as i32;
    assert!(pid > 0, "spawned process id must be positive");
    write_pid(dojo_home, pid)?;
    Ok(())
}

pub async fn run_worker(dojo_home: &Path) -> anyhow::Result<()> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let dojo_home = dojo_home
        .canonicalize()
        .with_context(|| format!("canonicalize {}", dojo_home.display()))?;
    let self_pid = std::process::id() as i32;
    assert!(self_pid > 0, "self pid must be positive");
    write_pid(&dojo_home, self_pid)?;

    let client = Client::new();
    let jj_repo_folder = config::default_workspace_jj_repo_folder(&dojo_home)?;
    assert!(jj_repo_folder.is_dir(), "jj_repo_folder must be a directory");
    let mut watcher = match create_operations_watcher(&jj_repo_folder) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("dojjo background sync: failed to watch operations dir: {e:#}");
            None
        }
    };
    let interval = Duration::from_secs(BACKGROUND_SYNC_INTERVAL_SECS);
    let mut last_sync_started: Option<Instant> = None;
    let mut next_poll_due = Instant::now();
    let mut local_change_pending = false;

    loop {
        let cfg = config::load_or_create_repo_machine_config(&dojo_home)?;
        if !cfg.background_sync_enabled {
            clear_pid(&dojo_home)?;
            return Ok(());
        }

        if let Some(w) = watcher.as_mut() {
            if drain_operations_events(w) {
                local_change_pending = true;
            }
        }

        let now = Instant::now();
        let poll_due = now >= next_poll_due;
        let debounce_due = match last_sync_started {
            Some(t) => now.duration_since(t) >= interval,
            None => true,
        };
        let sync_due = poll_due || (local_change_pending && debounce_due);
        if sync_due {
            let started_at = Instant::now();
            if let Err(e) = sync::run_for_dojo_home(&client, &dojo_home).await {
                eprintln!("dojjo background sync: {e:#}");
            } else {
                local_change_pending = false;
            }
            last_sync_started = Some(started_at);
            next_poll_due = started_at + interval;
            continue;
        }

        let mut wake_at = next_poll_due;
        if local_change_pending {
            let debounce_at = last_sync_started
                .map(|t| t + interval)
                .unwrap_or_else(Instant::now);
            if debounce_at < wake_at {
                wake_at = debounce_at;
            }
        }
        let sleep_for = wake_at.saturating_duration_since(Instant::now());
        if let Some(w) = watcher.as_mut() {
            tokio::select! {
                maybe_event = w.rx.recv() => {
                    if let Some(Ok(_event)) = maybe_event {
                        local_change_pending = true;
                    }
                }
                _ = tokio::time::sleep(sleep_for) => {}
            }
        } else {
            tokio::time::sleep(sleep_for).await;
        }
    }
}

pub fn join_or_create_status_message(dojo_home: &Path) -> anyhow::Result<String> {
    assert!(dojo_home.as_os_str().len() > 0, "dojo_home must not be empty");
    let (cfg, _running) = status_for_dojo_home(dojo_home)?;
    if cfg.background_sync_enabled {
        return Ok("Repo is now syncing automatically in the background.".to_string());
    }
    Ok("Repo syncing is disabled. Use `dojjo background-sync enable` to enable.".to_string())
}
