//! Git worktree lifecycle management.
//!
//! When `per_repo_limit > 1`, each agent gets its own worktree so that multiple
//! agents can work concurrently on the same repository without git index
//! conflicts. The worktree is automatically cleaned up when dropped.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Manages a git worktree for an isolated agent workspace.
pub struct Worktree {
    /// Path to the created worktree directory.
    pub path: PathBuf,
    /// Path to the main repository.
    repo_path: PathBuf,
}

impl Worktree {
    /// Create a new worktree for the given branch.
    ///
    /// Runs `git worktree add <base_dir>/<branch_slug> <branch>`.
    pub fn create(repo_path: &Path, branch: &str, base_dir: &Path) -> Result<Self> {
        let slug: String = branch
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let worktree_path = base_dir.join(&slug);

        std::fs::create_dir_all(base_dir).context("failed to create worktree base directory")?;

        let output = Command::new("git")
            .args(["worktree", "add", worktree_path.to_str().unwrap(), branch])
            .current_dir(repo_path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .output()
            .context("failed to run git worktree add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git worktree add failed: {stderr}");
        }

        tracing::info!(
            worktree = %worktree_path.display(),
            branch,
            "created git worktree"
        );

        Ok(Self {
            path: worktree_path,
            repo_path: repo_path.to_path_buf(),
        })
    }

    /// Remove the worktree.
    pub fn cleanup(&self) -> Result<()> {
        let output = Command::new("git")
            .args(["worktree", "remove", "--force", self.path.to_str().unwrap()])
            .current_dir(&self.repo_path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .output()
            .context("failed to run git worktree remove")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(worktree = %self.path.display(), "worktree removal failed: {stderr}");
        } else {
            tracing::info!(worktree = %self.path.display(), "removed git worktree");
        }

        Ok(())
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.path.exists()
            && let Err(e) = self.cleanup()
        {
            tracing::warn!(error = %e, "best-effort worktree cleanup failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Create a temporary git repo for testing.
    ///
    /// Strips environment variables that may leak from the outer repo
    /// (e.g. `GIT_DIR`, `GIT_INDEX_FILE`) so the fresh repo is fully
    /// isolated.
    fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        // Create an initial commit so HEAD exists
        Command::new("git")
            .args(["commit", "--allow-empty", "-m", "initial"])
            .current_dir(dir.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        // Create a branch to attach the worktree to
        Command::new("git")
            .args(["branch", "test-branch"])
            .current_dir(dir.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        dir
    }

    #[test]
    fn create_and_cleanup_worktree() {
        let repo_dir = init_test_repo();
        let base = tempfile::tempdir().unwrap();

        let wt = Worktree::create(repo_dir.path(), "test-branch", base.path()).unwrap();
        assert!(
            wt.path.exists(),
            "worktree directory should exist after create"
        );
        assert!(
            wt.path.join(".git").exists(),
            "worktree should have a .git file/dir"
        );

        let path = wt.path.clone();
        wt.cleanup().unwrap();
        assert!(
            !path.exists(),
            "worktree directory should be removed after cleanup"
        );
    }

    #[test]
    fn drop_cleans_up_worktree() {
        let repo_dir = init_test_repo();
        let base = tempfile::tempdir().unwrap();

        let wt = Worktree::create(repo_dir.path(), "test-branch", base.path()).unwrap();
        let path = wt.path.clone();
        assert!(path.exists());

        drop(wt);
        assert!(!path.exists(), "drop should auto-cleanup the worktree");
    }

    #[test]
    fn branch_slug_normalizes_special_chars() {
        let slug: String = "auto/TASK-42/foo/bar"
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        assert_eq!(slug, "auto_TASK-42_foo_bar");
    }
}
