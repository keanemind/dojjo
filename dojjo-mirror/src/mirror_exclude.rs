//! Machine-local paths under `.jj/repo` that must never be replicated by the JJ file mirror.
//!
//! Shared repo state lives in views, the op store, and the object store. `workspace_store`
//! maps workspace names to **this host's** working-copy roots; replicating it would apply
//! another machine's filesystem paths locally. `store/git_target` is host-local too: each
//! machine points at its own Git store (`git` → `store/git/` on clients). `config-id` is
//! JJ secure-config identity (random id → `~/.config/jj/repos/<id>/`); each dojjo peer's
//! sync host is a distinct repo path, so peers legitimately have different ids.
//!
//! JJ `FileLock` paths under `.jj/repo` are ephemeral process coordination files; they must
//! never be uploaded, published in the manifest, or served. Paths are listed explicitly in
//! [`JJ_REPO_FILE_LOCK_PATHS`] — not by filename heuristic. When upgrading the JJ reference
//! tree (`jj-<sha>/` from `mise run fetch-jj-ref`), re-audit `FileLock::lock` call
//! sites under `repo_path` and update that list.

/// Ephemeral `FileLock` files under `.jj/repo` (slash-separated, relative to repo root).
///
/// Maintain this list against the pinned JJ reference tree (`jj-<sha>/` from `mise run fetch-jj-ref`).
/// Search for `FileLock::lock` and record every lock path rooted at `repo_path` / `.jj/repo`.
///
/// | Path | JJ source |
/// |------|-----------|
/// | `store/extra/lock` | `lib/src/stacked_table.rs` — `TableStore::lock` (git extra metadata at `store/extra/`) |
/// | `op_heads/lock` | `lib/src/simple_op_heads_store.rs` — `SimpleOpHeadsStore::lock` |
/// | `workspace_store/index.lock` | `lib/src/workspace_store.rs` — `index` with `.lock` extension (tree also excluded) |
/// | `git_import_export.lock` | `cli/src/cli_util.rs` — `lock_git_import_export` |
///
/// Not listed: `working_copy.lock` is under the workspace checkout (`.jj/working_copy/`), not `.jj/repo`.
pub const JJ_REPO_FILE_LOCK_PATHS: &[&str] = &[
    "git_import_export.lock",
    "op_heads/lock",
    "store/extra/lock",
    "workspace_store/index.lock",
];

/// `true` when `rel` (slash-separated, relative to `.jj/repo`) must not appear in the mirror.
pub fn jj_repo_rel_excluded_from_mirror(rel: &str) -> bool {
    assert!(!rel.is_empty(), "rel must not be empty");
    assert!(
        !rel.starts_with('/'),
        "rel must be relative (no leading slash): {rel:?}"
    );
    assert!(
        !rel.contains('\\'),
        "rel must use forward slashes only: {rel:?}"
    );

    if rel == "workspace_store" || rel.starts_with("workspace_store/") {
        return true;
    }
    if rel == "store/git_target" {
        return true;
    }
    // Symlink when present as a file path (walk normally skips symlinks; defensive).
    if rel == "config.toml" {
        return true;
    }
    if rel == "config-id" {
        return true;
    }
    JJ_REPO_FILE_LOCK_PATHS.contains(&rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_workspace_store_tree() {
        assert!(jj_repo_rel_excluded_from_mirror("workspace_store"));
        assert!(jj_repo_rel_excluded_from_mirror(
            "workspace_store/workspaces"
        ));
    }

    #[test]
    fn does_not_exclude_shared_repo_paths() {
        assert!(!jj_repo_rel_excluded_from_mirror("op_store/views/abc"));
        assert!(!jj_repo_rel_excluded_from_mirror("store/extra/foo.bin"));
    }

    #[test]
    fn excludes_repo_config_toml_symlink_target() {
        assert!(jj_repo_rel_excluded_from_mirror("config.toml"));
    }

    #[test]
    fn excludes_git_target() {
        assert!(jj_repo_rel_excluded_from_mirror("store/git_target"));
    }

    #[test]
    fn excludes_repo_config_id() {
        assert!(jj_repo_rel_excluded_from_mirror("config-id"));
    }

    #[test]
    fn excludes_every_listed_jj_file_lock_path() {
        for path in JJ_REPO_FILE_LOCK_PATHS {
            assert!(
                jj_repo_rel_excluded_from_mirror(path),
                "listed lock path must be excluded: {path}"
            );
        }
    }

    #[test]
    fn does_not_exclude_unlisted_lock_like_paths() {
        assert!(!jj_repo_rel_excluded_from_mirror("store/extra/lockfile"));
        assert!(!jj_repo_rel_excluded_from_mirror("op_heads/locks"));
        assert!(!jj_repo_rel_excluded_from_mirror("store/extra/foo.lock"));
    }

    #[test]
    fn jj_repo_file_lock_paths_are_sorted_unique() {
        let mut sorted = JJ_REPO_FILE_LOCK_PATHS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted,
            JJ_REPO_FILE_LOCK_PATHS,
            "JJ_REPO_FILE_LOCK_PATHS must stay sorted and deduplicated for reviewability"
        );
    }
}
