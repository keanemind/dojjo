//! `dojjo init` — write global client config (`api_base` for the sync-server).

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::Context;

use crate::config::{self, GlobalConfig};

const DEFAULT_API_BASE: &str = "http://localhost:3000/api";

fn prompt_api_base() -> anyhow::Result<String> {
    assert!(
        io::stdout().is_terminal(),
        "prompt requires a terminal; use --api-base"
    );
    print!("Dojjo sync-server API URL [{DEFAULT_API_BASE}]: ");
    io::stdout().flush().context("flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("read stdin")?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_API_BASE.to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_api_base(api_base: &str) -> anyhow::Result<String> {
    let api_base = config::normalize_api_base(api_base);
    anyhow::ensure!(!api_base.is_empty(), "API base URL must not be empty");
    anyhow::ensure!(
        api_base.starts_with("http://") || api_base.starts_with("https://"),
        "API base URL must start with http:// or https:// (got {api_base:?})"
    );
    if !api_base.ends_with("/api") {
        eprintln!(
            "note: API URL usually ends with /api (e.g. http://your-host:3000/api); got {api_base}"
        );
    }
    Ok(api_base)
}

pub fn run(api_base: Option<&str>) -> anyhow::Result<()> {
    let api_base = match api_base {
        Some(s) => validate_api_base(s)?,
        None => {
            if io::stdin().is_terminal() {
                validate_api_base(&prompt_api_base()?)?
            } else {
                anyhow::bail!(
                    "not a terminal; pass --api-base (e.g. http://your-host:3000/api)"
                );
            }
        }
    };

    let cfg = GlobalConfig { api_base: api_base.clone() };
    config::save_global_config(&cfg).context("save global config")?;

    let path = config::global_config_path().context("global config path")?;
    println!("configured dojjo; global config {}", path.display());
    println!("api_base {api_base}");
    Ok(())
}
