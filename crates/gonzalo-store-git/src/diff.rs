//! Git working-tree diffing for incremental code-graph sync (gonzalo#93).
//!
//! The indexer's default path is a full reconcile: re-walk and re-parse the
//! whole tree every run. When the source is a git repo, [`changed_paths`] sources
//! the changed set directly from git — the tree of a recorded *base* commit
//! against the live working tree — so only added/modified files need re-parsing
//! and deleted files are dropped. The classification is uniform A/M/D, matching
//! [`gonzalo_core::Manifest::reconcile`]'s set model, so a periodic full reconcile
//! still converges the view if any incremental event is ever missed.

use gonzalo_core::{CoreError, Result};
use std::path::Path;

/// Source paths that changed between a git base commit and the current working
/// tree, classified uniformly as added / modified / deleted. Paths are
/// repo-relative with `/` separators and each vector is sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangedPaths {
    /// Paths present in the working tree but not in the base commit (includes
    /// untracked files).
    pub added: Vec<String>,
    /// Paths present in both whose content differs.
    pub modified: Vec<String>,
    /// Paths present in the base commit but gone from the working tree.
    pub deleted: Vec<String>,
}

impl ChangedPaths {
    /// True when nothing changed between the base commit and the working tree.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// True if `path` is inside a git working tree (has a discoverable repository).
pub fn is_git_repo(path: &Path) -> bool {
    git2::Repository::discover(path).is_ok()
}

/// Resolve the `HEAD` of the repo at `root` to its commit SHA (hex). This is the
/// value to record as the base after a successful index, so the next run can diff
/// against it incrementally.
pub fn head_commit(root: &Path) -> Result<String> {
    let repo = git2::Repository::open(root).map_err(|e| CoreError::Backend(e.to_string()))?;
    let head = repo.head().map_err(|e| CoreError::Backend(e.to_string()))?;
    let oid = head
        .target()
        .ok_or_else(|| CoreError::Backend("HEAD has no target commit".into()))?;
    Ok(oid.to_string())
}

/// Compute the changed path set between the tree of commit `base` and the current
/// working tree of the repo at `root`. Untracked files count as additions;
/// renames are surfaced as a delete of the old path plus an add of the new one
/// (uniform A/M/D — no rename tracking). Paths are repo-relative, `/`-separated,
/// and sorted.
pub fn changed_paths(root: &Path, base: &str) -> Result<ChangedPaths> {
    let repo = git2::Repository::open(root).map_err(|e| CoreError::Backend(e.to_string()))?;
    let base_oid =
        git2::Oid::from_str(base).map_err(|e| CoreError::Backend(format!("bad base oid: {e}")))?;
    let base_tree = repo
        .find_commit(base_oid)
        .map_err(|e| CoreError::Backend(e.to_string()))?
        .tree()
        .map_err(|e| CoreError::Backend(e.to_string()))?;

    let mut opts = git2::DiffOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut opts))
        .map_err(|e| CoreError::Backend(e.to_string()))?;

    let mut changed = ChangedPaths::default();
    for delta in diff.deltas() {
        match delta.status() {
            git2::Delta::Added | git2::Delta::Untracked | git2::Delta::Copied => {
                if let Some(p) = path_str(delta.new_file().path()) {
                    changed.added.push(p);
                }
            }
            git2::Delta::Deleted => {
                if let Some(p) = path_str(delta.old_file().path()) {
                    changed.deleted.push(p);
                }
            }
            git2::Delta::Modified | git2::Delta::Typechange => {
                if let Some(p) = path_str(delta.new_file().path()) {
                    changed.modified.push(p);
                }
            }
            git2::Delta::Renamed => {
                if let Some(p) = path_str(delta.old_file().path()) {
                    changed.deleted.push(p);
                }
                if let Some(p) = path_str(delta.new_file().path()) {
                    changed.added.push(p);
                }
            }
            _ => {}
        }
    }
    changed.added.sort();
    changed.added.dedup();
    changed.modified.sort();
    changed.modified.dedup();
    changed.deleted.sort();
    changed.deleted.dedup();
    Ok(changed)
}

/// Render a git delta path as a `/`-separated relative string.
fn path_str(path: Option<&Path>) -> Option<String> {
    path.map(|p| p.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    /// Init a repo, write `files`, commit them, and return the commit SHA.
    fn init_repo_with(dir: &Path, files: &[(&str, &str)]) -> String {
        let repo = git2::Repository::init(dir).unwrap();
        for (name, contents) in files {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
        }
        commit_all(&repo, "initial")
    }

    fn commit_all(repo: &git2::Repository, msg: &str) -> String {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = git2::Signature::now("t", "t@localhost").unwrap();
        let parent = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap();
        oid.to_string()
    }

    #[test]
    fn is_git_repo_detects_repo() {
        let dir = TempDir::new().unwrap();
        assert!(!is_git_repo(dir.path()), "empty dir is not a repo");
        git2::Repository::init(dir.path()).unwrap();
        assert!(is_git_repo(dir.path()), "initialized dir is a repo");
    }

    #[test]
    fn head_commit_resolves_to_the_last_commit() {
        let dir = TempDir::new().unwrap();
        let sha = init_repo_with(dir.path(), &[("a.rs", "fn a() {}")]);
        assert_eq!(head_commit(dir.path()).unwrap(), sha);
    }

    #[test]
    fn changed_paths_reports_added_modified_deleted() {
        let dir = TempDir::new().unwrap();
        let base = init_repo_with(dir.path(), &[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);

        // Modify a.rs, delete b.rs, add an untracked c.rs.
        std::fs::write(dir.path().join("a.rs"), "fn a() { x(); }").unwrap();
        std::fs::remove_file(dir.path().join("b.rs")).unwrap();
        std::fs::write(dir.path().join("c.rs"), "fn c() {}").unwrap();

        let changed = changed_paths(dir.path(), &base).unwrap();
        assert_eq!(changed.added, vec!["c.rs".to_string()]);
        assert_eq!(changed.modified, vec!["a.rs".to_string()]);
        assert_eq!(changed.deleted, vec!["b.rs".to_string()]);
        assert!(!changed.is_empty());
    }

    #[test]
    fn changed_paths_empty_when_working_tree_matches_base() {
        let dir = TempDir::new().unwrap();
        let base = init_repo_with(dir.path(), &[("a.rs", "fn a() {}")]);
        let changed = changed_paths(dir.path(), &base).unwrap();
        assert!(changed.is_empty(), "clean tree has no changes: {changed:?}");
    }

    #[test]
    fn changed_paths_finds_nested_untracked_files() {
        let dir = TempDir::new().unwrap();
        let base = init_repo_with(dir.path(), &[("a.rs", "fn a() {}")]);
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.rs"), "fn d() {}").unwrap();

        let changed = changed_paths(dir.path(), &base).unwrap();
        assert_eq!(changed.added, vec!["sub/deep.rs".to_string()]);
    }
}
