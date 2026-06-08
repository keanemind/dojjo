//! Run `jj` subprocesses for workspace lifecycle (delegate semantics to Jujutsu).

use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::Context;

pub fn jj_command() -> &'static str {
    "jj"
}

pub fn run_jj(cwd: &Path, args: &[&str]) -> anyhow::Result<Output> {
    assert!(cwd.as_os_str().len() > 0, "cwd must not be empty");
    assert!(!args.is_empty(), "jj args must not be empty");
    let out = std::process::Command::new(jj_command())
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("spawn jj {}", args.join(" ")))?;
    assert!(out.status.code().is_some(), "jj must exit with a status code");
    Ok(out)
}

/// Hex operation id for the repo's current op head (`jj op log -n 1`).
///
/// Uses `--ignore-working-copy` so sync can read the global op id without
/// snapshotting the query workspace's working copy (which would create spurious
/// operations and can wedge sentinel workspaces like `_default`).
pub fn workspace_current_operation_id_hex(workspace: &Path) -> anyhow::Result<String> {
    let out = run_jj(
        workspace,
        &[
            "op",
            "log",
            "-n",
            "1",
            "--ignore-working-copy",
            "--no-graph",
            "-T",
            "id",
        ],
    )?;
    anyhow::ensure!(
        out.status.success(),
        "jj op log failed (cwd {}): {}",
        workspace.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(!id.is_empty(), "jj op log returned empty operation id");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit()),
        "operation id must be hex"
    );
    Ok(id)
}

pub fn run_jj_ok(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = run_jj(cwd, args)?;
    anyhow::ensure!(
        out.status.success(),
        "jj {} failed (cwd {}): {}",
        args.join(" "),
        cwd.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// Run a `jj` subcommand against another workspace via `-R/--repository`.
pub fn run_jj_with_repository(repo_workspace: &Path, args: &[&str]) -> anyhow::Result<()> {
    assert!(
        repo_workspace.as_os_str().len() > 0,
        "repo_workspace must not be empty"
    );
    assert!(!args.is_empty(), "jj args must not be empty");
    let repo = repo_workspace
        .to_str()
        .context("repository workspace path must be UTF-8")?;
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + 2);
    full.push("-R");
    full.push(repo);
    full.extend_from_slice(args);
    run_jj_ok(repo_workspace, &full)
}

/// Sparse patterns from `jj sparse list` (empty vec = completely sparse checkout).
pub fn workspace_sparse_list(workspace: &Path) -> anyhow::Result<Vec<String>> {
    assert!(workspace.as_os_str().len() > 0, "workspace must not be empty");
    let out = run_jj(workspace, &["sparse", "list"])?;
    anyhow::ensure!(
        out.status.success(),
        "jj sparse list failed (cwd {}): {}",
        workspace.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let patterns: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    Ok(patterns)
}

/// Clear all sparse patterns so no project files are present in the working copy.
pub fn workspace_set_sparse_empty(workspace: &Path) -> anyhow::Result<()> {
    assert!(workspace.as_os_str().len() > 0, "workspace must not be empty");
    run_jj_ok(workspace, &["sparse", "set", "--clear"])
}

/// `jj workspace add` with empty sparse patterns and `@` on `root()`.
pub fn workspace_add_frozen_default(
    from_workspace: &Path,
    dest_workspace_root: &Path,
) -> anyhow::Result<()> {
    let dest = dest_workspace_root
        .to_str()
        .context("dest path must be UTF-8")?;
    run_jj_ok(
        from_workspace,
        &[
            "workspace",
            "add",
            dest,
            "--name",
            crate::config::DEFAULT_WORKSPACE_NAME,
            "--sparse-patterns",
            "empty",
        ],
    )?;
    run_jj_ok(dest_workspace_root, &["new", "root()"])
}

pub fn workspace_add_with_repository(
    repository_workspace: &Path,
    dest_workspace_root: &Path,
    name: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(!name.is_empty(), "workspace name must not be empty");
    let dest = dest_workspace_root
        .to_str()
        .context("dest path must be UTF-8")?;
    run_jj_with_repository(
        repository_workspace,
        &["workspace", "add", dest, "--name", name],
    )
}

/// Move a colocated workspace `.git` into `.jj/repo/store/git` and set `git_target` to `git`.
pub fn git_colocation_disable(workspace: &Path) -> anyhow::Result<()> {
    run_jj_ok(workspace, &["git", "colocation", "disable"])
}

pub fn workspace_rename(from_workspace: &Path, new_name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!new_name.is_empty(), "new workspace name must not be empty");
    run_jj_ok(from_workspace, &["workspace", "rename", new_name])
}

/// True when `jj workspace list` output contains a line for `name`.
pub fn workspace_exists(from_workspace: &Path, name: &str) -> anyhow::Result<bool> {
    let out = run_jj(from_workspace, &["workspace", "list"])?;
    parse_workspace_list_contains_name(&out, name)
}

/// True when `jj -R <repo> --ignore-working-copy workspace list` contains `name`.
pub fn workspace_exists_at_repository(repo_workspace: &Path, name: &str) -> anyhow::Result<bool> {
    let out = run_jj(
        repo_workspace,
        &[
            "-R",
            repo_workspace
                .to_str()
                .context("repository workspace path must be UTF-8")?,
            "--ignore-working-copy",
            "workspace",
            "list",
        ],
    )?;
    parse_workspace_list_contains_name(&out, name)
}

fn default_workspace_list_contains_name(text: &str, name: &str) -> bool {
    assert!(!name.is_empty(), "workspace name must not be empty");
    text.lines().any(|line| {
        let token = line.split_whitespace().next().unwrap_or("");
        token.trim_end_matches(':') == name
    })
}

fn parse_workspace_list_contains_name(out: &std::process::Output, name: &str) -> anyhow::Result<bool> {
    let text = parse_workspace_list_stdout(out)?;
    Ok(default_workspace_list_contains_name(&text, name))
}

fn parse_workspace_list_stdout(out: &std::process::Output) -> anyhow::Result<String> {
    anyhow::ensure!(
        out.status.success(),
        "jj workspace list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Template for `jj workspace list -T`. Each row must end with `\n`; non-current rows
/// still emit a trailing `\t` before the newline.
pub const WORKSPACE_LIST_TEMPLATE: &str =
    r#"name ++ "\t" ++ if(target.current_working_copy(), "current", "") ++ "\n""#;

/// One row from [`WORKSPACE_LIST_TEMPLATE`] output (`name`, tab, optional `current`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceListEntry {
    pub name: String,
    pub is_current: bool,
}

/// Parse stdout from [`WORKSPACE_LIST_TEMPLATE`].
fn parse_workspace_list_template(text: &str) -> anyhow::Result<Vec<WorkspaceListEntry>> {
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (name, flag) = line
            .split_once('\t')
            .with_context(|| format!("parse workspace list line {line:?}"))?;
        let name = name.trim().trim_end_matches(':').to_string();
        anyhow::ensure!(!name.is_empty(), "workspace name must not be empty in line {line:?}");
        let is_current = flag.trim() == "current";
        entries.push(WorkspaceListEntry { name, is_current });
    }
    assert!(entries.len() <= text.lines().count(), "entries cannot exceed input lines");
    anyhow::ensure!(
        !entries.is_empty(),
        "workspace list template output must not be empty"
    );
    Ok(entries)
}

fn select_current_workspace_name(entries: &[WorkspaceListEntry]) -> anyhow::Result<String> {
    let mut current = entries.iter().filter(|e| e.is_current);
    let first = current.next().context("no current workspace in jj workspace list")?;
    if current.next().is_some() {
        anyhow::bail!("multiple current workspaces in jj workspace list");
    }
    Ok(first.name.clone())
}

pub fn list_workspaces(from_workspace: &Path) -> anyhow::Result<Vec<WorkspaceListEntry>> {
    let out = run_jj(
        from_workspace,
        &["workspace", "list", "-T", WORKSPACE_LIST_TEMPLATE],
    )?;
    let text = parse_workspace_list_stdout(&out)?;
    parse_workspace_list_template(&text)
}

pub fn current_workspace_name(from_workspace: &Path) -> anyhow::Result<String> {
    let entries = list_workspaces(from_workspace)?;
    select_current_workspace_name(&entries)
}

/// Canonical workspace root for `name` (`jj workspace root --name`).
pub fn workspace_root_for_name(from_workspace: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let root = try_workspace_root_for_name(from_workspace, name)?
        .with_context(|| format!("workspace {name} has no recorded path on this machine"))?;
    Ok(root)
}

/// Like [`workspace_root_for_name`], but returns `None` when jj has no local path for `name`.
pub fn try_workspace_root_for_name(
    from_workspace: &Path,
    name: &str,
) -> anyhow::Result<Option<PathBuf>> {
    anyhow::ensure!(!name.is_empty(), "workspace name must not be empty");
    let out = run_jj(from_workspace, &["workspace", "root", "--name", name])?;
    if out.status.success() {
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        anyhow::ensure!(!path.is_empty(), "jj workspace root returned empty path");
        let path = PathBuf::from(path);
        return path
            .canonicalize()
            .map(Some)
            .with_context(|| format!("canonicalize workspace root {}", path.display()));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Workspace has no recorded path")
        || stderr.contains("Cannot resolve absolute workspace path")
    {
        return Ok(None);
    }
    anyhow::bail!(
        "jj workspace root --name {name} failed: {}",
        stderr.trim()
    );
}

pub fn workspace_forget(from_workspace: &Path, name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!name.is_empty(), "workspace name must not be empty");
    run_jj_ok(from_workspace, &["workspace", "forget", name])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::sync::OnceLock;
    use tempfile::tempdir;

    fn jj_available() -> bool {
        static JJ: OnceLock<bool> = OnceLock::new();
        *JJ.get_or_init(|| {
            Command::new(jj_command())
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
    }

    #[test]
    fn default_workspace_list_contains_name_parses_jj_format() {
        let text = "default: abc123 (empty) (no description set)\npeer: def456 (empty)\n";
        assert!(default_workspace_list_contains_name(text, "default"));
        assert!(default_workspace_list_contains_name(text, "peer"));
        assert!(!default_workspace_list_contains_name(text, "missing"));
    }

    #[test]
    fn parse_template_multiline_with_trailing_tabs() {
        // Real `jj workspace list -T` output: non-current rows end with `\t\n`.
        let text = "default\t\nmac-lan\t\nmac-lan-b\t\nmac-lan-c\tcurrent\n";
        let entries = parse_workspace_list_template(text).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries,
            vec![
                WorkspaceListEntry {
                    name: "default".into(),
                    is_current: false,
                },
                WorkspaceListEntry {
                    name: "mac-lan".into(),
                    is_current: false,
                },
                WorkspaceListEntry {
                    name: "mac-lan-b".into(),
                    is_current: false,
                },
                WorkspaceListEntry {
                    name: "mac-lan-c".into(),
                    is_current: true,
                },
            ]
        );
        assert_eq!(
            select_current_workspace_name(&entries).unwrap(),
            "mac-lan-c"
        );
    }

    #[test]
    fn parse_template_non_current_row_has_trailing_tab() {
        let row = "default\t";
        assert_eq!(row.trim(), "default", "trim removes tab — must not trim before split");
        let entries = parse_workspace_list_template(&format!("{row}\n")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "default");
        assert!(!entries[0].is_current);
    }

    #[test]
    fn parse_template_rejects_line_trimmed_before_split() {
        // Simulates `line.trim()` before `split_once('\t')` on a non-current row.
        let err = parse_workspace_list_template("default\n").unwrap_err();
        assert!(
            err.to_string().contains("parse workspace list line"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_template_missing_newlines_yields_no_current() {
        // Template without `\n` concatenates all workspaces onto one line.
        let text = "default\tmac-lan\tmac-lan-b\tmac-lan-c\tcurrent\n";
        let entries = parse_workspace_list_template(text).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_current);
        assert!(select_current_workspace_name(&entries).is_err());
    }

    #[test]
    fn select_current_workspace_name_errors() {
        let none = vec![WorkspaceListEntry {
            name: "default".into(),
            is_current: false,
        }];
        assert!(select_current_workspace_name(&none).is_err());

        let two = vec![
            WorkspaceListEntry {
                name: "a".into(),
                is_current: true,
            },
            WorkspaceListEntry {
                name: "b".into(),
                is_current: true,
            },
        ];
        let err = select_current_workspace_name(&two).unwrap_err();
        assert!(err.to_string().contains("multiple current"));
    }

    #[test]
    fn workspace_list_template_includes_row_newline() {
        assert!(
            WORKSPACE_LIST_TEMPLATE.contains(r#"++ "\n""#),
            "template must terminate each workspace row with a newline"
        );
    }

    /// Exercises `list_workspaces` / `current_workspace_name` against a real `jj` binary.
    #[test]
    fn list_workspaces_against_real_jj() {
        if !jj_available() {
            eprintln!("skipping list_workspaces_against_real_jj: jj not in PATH");
            return;
        }

        let dir = tempdir().unwrap();
        let main = dir.path().join("main");
        let peer = dir.path().join("peer");
        fs::create_dir_all(&main).unwrap();

        if run_jj_ok(&main, &["git", "init"]).is_err() {
            eprintln!("skipping list_workspaces_against_real_jj: jj git init failed (sandbox?)");
            return;
        }
        fs::write(main.join("file"), "contents").unwrap();
        run_jj_ok(&main, &["commit", "-m", "init"]).unwrap();
        run_jj_ok(
            &main,
            &["workspace", "add", peer.to_str().unwrap(), "--name", "peer"],
        )
        .unwrap();

        let main_entries = list_workspaces(&main).unwrap();
        assert_eq!(main_entries.len(), 2);
        assert_eq!(select_current_workspace_name(&main_entries).unwrap(), "default");

        let peer_entries = list_workspaces(&peer).unwrap();
        assert_eq!(peer_entries.len(), 2);
        assert_eq!(select_current_workspace_name(&peer_entries).unwrap(), "peer");

        assert_eq!(current_workspace_name(&main).unwrap(), "default");
        assert_eq!(current_workspace_name(&peer).unwrap(), "peer");
    }
}
