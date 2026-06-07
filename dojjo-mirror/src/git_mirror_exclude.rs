//! Paths under JJ's internal `store/git/` that must not be mirrored as independent files.
//!
//! The entire Git repository (objects, refs, config, HEAD, …) is moved with `git fetch` /
//! `git push` against the per-dojo bare remote. `store/git_target` is host-local and excluded
//! separately via [`crate::mirror_exclude::jj_repo_rel_excluded_from_mirror`].

/// `true` when `rel` (slash-separated, relative to `.jj/repo`) must be excluded from the JJ file mirror.
pub fn jj_repo_rel_excluded_from_git_file_mirror(rel: &str) -> bool {
    assert!(!rel.is_empty(), "rel must not be empty");
    assert!(
        !rel.starts_with('/'),
        "rel must be relative (no leading slash): {rel:?}"
    );
    assert!(
        !rel.contains('\\'),
        "rel must use forward slashes only: {rel:?}"
    );

    rel == "store/git" || rel.starts_with("store/git/")
}

/// `true` for paths under `store/git/` that may exist locally after Git transport but are never mirror blobs.
pub fn jj_repo_rel_git_transport_local_state(rel: &str) -> bool {
    jj_repo_rel_excluded_from_git_file_mirror(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_entire_git_store_tree() {
        assert!(jj_repo_rel_excluded_from_git_file_mirror("store/git"));
        assert!(jj_repo_rel_excluded_from_git_file_mirror(
            "store/git/objects/ab/cdef1234"
        ));
        assert!(jj_repo_rel_excluded_from_git_file_mirror(
            "store/git/refs/heads/main"
        ));
        assert!(jj_repo_rel_excluded_from_git_file_mirror(
            "store/git/refs/jj/keep/deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        ));
        assert!(jj_repo_rel_excluded_from_git_file_mirror(
            "store/git/packed-refs"
        ));
        assert!(jj_repo_rel_excluded_from_git_file_mirror("store/git/config"));
        assert!(jj_repo_rel_excluded_from_git_file_mirror("store/git/HEAD"));
        assert!(jj_repo_rel_excluded_from_git_file_mirror("store/git/FETCH_HEAD"));
    }

    #[test]
    fn does_not_exclude_git_target_or_extra() {
        assert!(!jj_repo_rel_excluded_from_git_file_mirror("store/git_target"));
        assert!(!jj_repo_rel_excluded_from_git_file_mirror(
            "store/extra/foo.bin"
        ));
    }
}
