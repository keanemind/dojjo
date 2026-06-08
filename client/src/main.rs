//! `dojjo` — create a dojo, join with a new JJ workspace, and sync the local repo under `_default`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod config;
mod create;
mod background_sync;
mod git_sync;
mod init_cmd;
mod jj_exec;
mod jj_workspace_store;
mod join;
mod manifest;
mod mirror_pull;
mod repo_walk;
mod smoke;
mod status;
mod sync;
mod sync_debug;
mod tus;
mod undojjo;
mod workspace_lifecycle;

#[derive(Parser)]
#[command(name = "dojjo")]
#[command(about = "Dojjo: sync Jujutsu .jj/repo state via a dojo server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Configure the sync-server API URL (run once per machine).
    Init {
        /// API base URL (e.g. `http://100.x.x.x:3000/api`). Prompted when omitted in a TTY.
        #[arg(long)]
        api_base: Option<String>,
    },
    /// Create a new dojo and upload from the current JJ workspace.
    ///
    /// Background sync is enabled by default for this dojo on first create.
    Create {
        /// Override the API base from `~/.dojjo/config.json`.
        #[arg(long)]
        api_base: Option<String>,
    },
    /// Print local and server diagnostics for the current workspace (always exits 0).
    Status,
    /// Remove local dojjo linkage; repo ends at this directory as JJ `default`.
    ///
    /// Other workspaces on this DOJJO_HOME get updated `.jj/repo` pointers. Server dojo unchanged.
    Undojjo,
    /// Join a dojo: add a workspace at `--into` backed by this machine's local repo.
    ///
    /// First join on this machine pulls the server mirror into `~/.dojjo/dojos/{id}/_default`.
    /// When the dojo is already set up locally, join only runs `jj workspace add` (warm join).
    /// Do not run from inside an existing Jujutsu workspace; `--into` must be empty.
    /// Background sync is enabled by default on first create/join of this dojo on this machine.
    Join {
        #[arg(long)]
        dojo_id: String,
        /// Workspace root directory (the new checkout; must not have `.jj` yet).
        #[arg(long)]
        into: PathBuf,
        /// JJ workspace name (defaults to hostname).
        #[arg(long)]
        name: Option<String>,
        /// Override the API base from `~/.dojjo/config.json`.
        #[arg(long)]
        api_base: Option<String>,
    },
    /// Manage continuous background sync for this repo on this machine.
    ///
    /// Default is enabled on first create/join. Disable only for troubleshooting.
    BackgroundSync {
        #[command(subcommand)]
        cmd: BackgroundSyncCmd,
    },
    /// Development helpers (smoke test, manual sync).
    Dev {
        #[command(subcommand)]
        cmd: DevCmd,
    },
}

#[derive(Subcommand)]
enum DevCmd {
    /// Push/pull the local repo (run from any linked workspace for this dojo).
    Sync,
    /// Create dojo, upload a tiny blob, verify manifest + GETs + ETag (no local jj repo).
    Smoke,
    /// Continuous sync worker for a single dojo (internal).
    SyncWorker {
        #[arg(long)]
        dojo_home: PathBuf,
    },
}

#[derive(Subcommand)]
enum BackgroundSyncCmd {
    /// Enable continuous background sync for this dojo on this machine.
    Enable,
    /// Disable continuous background sync for this dojo on this machine.
    Disable,
    /// Show enable/running status for this dojo on this machine.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().expect("cwd must be available");

    match cli.command {
        Commands::Init { api_base } => init_cmd::run(api_base.as_deref())?,
        Commands::Status => {
            if let Err(e) = status::run(&cwd).await {
                eprintln!("dojjo status: {e:#}");
            }
        }
        Commands::Create { api_base } => {
            let api_base = config::resolve_api_base(api_base.as_deref())?;
            let client = reqwest::Client::new();
            create::run(&client, &cwd, &api_base).await?
        }
        Commands::Undojjo => undojjo::run(&cwd).await?,
        Commands::Join {
            dojo_id,
            into,
            name,
            api_base,
        } => {
            let api_base = config::resolve_api_base(api_base.as_deref())?;
            let client = reqwest::Client::new();
            join::run(
                &client,
                &dojo_id,
                &into,
                name.as_deref(),
                &api_base,
                &cwd,
            )
            .await?
        }
        Commands::BackgroundSync { cmd } => match cmd {
            BackgroundSyncCmd::Enable => {
                let dojo_home = background_sync::set_enabled_for_workspace(&cwd, true)?;
                background_sync::ensure_worker_running(&dojo_home)?;
                println!(
                    "background sync enabled for dojo at {}",
                    dojo_home.display()
                );
            }
            BackgroundSyncCmd::Disable => {
                let dojo_home = background_sync::set_enabled_for_workspace(&cwd, false)?;
                background_sync::stop_worker(&dojo_home)?;
                println!(
                    "background sync disabled for dojo at {}",
                    dojo_home.display()
                );
            }
            BackgroundSyncCmd::Status => {
                let (dojo_home, cfg, running) = background_sync::status_for_workspace(&cwd)?;
                println!("dojo {}", dojo_home.display());
                println!(
                    "background_sync_enabled={}",
                    if cfg.background_sync_enabled { "true" } else { "false" }
                );
                println!("background_sync_running={}", if running { "true" } else { "false" });
            }
        },
        Commands::Dev { cmd } => {
            let client = reqwest::Client::new();
            match cmd {
                DevCmd::Sync => {
                    config::require_global_config()?;
                    sync::run(&client, &cwd).await?
                }
                DevCmd::Smoke => {
                    config::require_global_config()?;
                    smoke::run(&client).await?
                }
                DevCmd::SyncWorker { dojo_home } => {
                    background_sync::run_worker(&dojo_home).await?
                }
            }
        }
    }

    Ok(())
}
