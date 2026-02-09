//! Git worktree lifecycle management.
//!
//! Not used in phase 1 (per-repo limit = 1, so agents use the main working
//! directory). Ready for phase 2 when per-repo > 1 requires isolated checkouts.

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
