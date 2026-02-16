use anyhow::{Context, Result};
use git2::{BranchType, MergeOptions, Repository, Signature};
use std::path::Path;

/// Git operations on a repository using libgit2.
pub struct GitRepo {
    repo: Repository,
}

impl GitRepo {
    /// Open an existing repository.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = Repository::open(path)
            .context(format!("failed to open git repo at {}", path.display()))?;
        Ok(Self { repo })
    }

    /// Get the current branch name.
    pub fn current_branch(&self) -> Result<String> {
        let head = self.repo.head()?;
        let name = head
            .shorthand()
            .context("HEAD is not a named branch")?
            .to_string();
        Ok(name)
    }

    /// Get the HEAD commit SHA.
    pub fn head_sha(&self) -> Result<String> {
        let head = self.repo.head()?;
        let oid = head.target().context("HEAD has no target")?;
        Ok(oid.to_string())
    }

    /// Create a new branch from the current HEAD.
    pub fn create_branch(&self, name: &str) -> Result<()> {
        let head_commit = self.repo.head()?.peel_to_commit()?;
        self.repo.branch(name, &head_commit, false)?;
        // Checkout the new branch
        let refname = format!("refs/heads/{name}");
        let obj = self.repo.revparse_single(&refname)?;
        self.repo.checkout_tree(&obj, None)?;
        self.repo.set_head(&refname)?;
        Ok(())
    }

    /// Create a branch ref without checking it out.
    ///
    /// Used when creating worktrees: the branch must exist as a ref but must
    /// NOT be checked out in the main working directory, otherwise
    /// `git worktree add` will fail with "already used by worktree".
    pub fn create_branch_detached(&self, name: &str) -> Result<()> {
        let head_commit = self.repo.head()?.peel_to_commit()?;
        self.repo.branch(name, &head_commit, false)?;
        Ok(())
    }

    /// Checkout an existing branch.
    pub fn checkout(&self, branch: &str) -> Result<()> {
        let refname = format!("refs/heads/{branch}");
        let obj = self.repo.revparse_single(&refname)?;
        self.repo.checkout_tree(&obj, None)?;
        self.repo.set_head(&refname)?;
        Ok(())
    }

    /// Check if the working directory is clean.
    pub fn is_clean(&self) -> Result<bool> {
        let statuses = self.repo.statuses(None)?;
        Ok(statuses.is_empty())
    }

    /// Check if HEAD has any commits beyond the default branch (main/master).
    pub fn has_commits_beyond_main(&self) -> Result<bool> {
        let main = self.default_branch()?;
        let main_ref = format!("refs/heads/{main}");
        let main_oid = self.repo.revparse_single(&main_ref)?.id();
        let head_oid = self.repo.head()?.target().context("HEAD has no target")?;
        if main_oid == head_oid {
            return Ok(false);
        }
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push(head_oid)?;
        revwalk.hide(main_oid)?;
        Ok(revwalk.next().is_some())
    }

    /// Check if a named branch has any commits beyond the default branch.
    pub fn branch_has_commits_beyond_main(&self, branch: &str) -> Result<bool> {
        let main = self.default_branch()?;
        let main_ref = format!("refs/heads/{main}");
        let branch_ref = format!("refs/heads/{branch}");
        let main_oid = self.repo.revparse_single(&main_ref)?.id();
        let branch_oid = self.repo.revparse_single(&branch_ref)?.id();
        if main_oid == branch_oid {
            return Ok(false);
        }
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push(branch_oid)?;
        revwalk.hide(main_oid)?;
        Ok(revwalk.next().is_some())
    }

    /// Get a diff summary between the default branch and HEAD.
    pub fn diff_summary(&self) -> Result<String> {
        let main = self.default_branch()?;
        let main_ref = format!("refs/heads/{main}");
        let main_commit = self.repo.revparse_single(&main_ref)?.peel_to_commit()?;
        let head_commit = self.repo.head()?.peel_to_commit()?;

        let main_tree = main_commit.tree()?;
        let head_tree = head_commit.tree()?;

        let diff = self
            .repo
            .diff_tree_to_tree(Some(&main_tree), Some(&head_tree), None)?;

        let stats = diff.stats()?;
        Ok(format!(
            "{} files changed, {} insertions(+), {} deletions(-)",
            stats.files_changed(),
            stats.insertions(),
            stats.deletions()
        ))
    }

    /// Get the full unified diff (patch) between the default branch and a named branch.
    ///
    /// Returns the diff as plain text in unified diff format, suitable for
    /// display in code review tools. Does not require checking out the branch.
    pub fn diff_patch_for_branch(&self, branch: &str) -> Result<String> {
        let main = self.default_branch()?;
        let main_ref = format!("refs/heads/{main}");
        let branch_ref = format!("refs/heads/{branch}");

        let main_commit = self
            .repo
            .revparse_single(&main_ref)?
            .peel_to_commit()
            .context(format!("failed to resolve default branch '{main}'"))?;
        let branch_commit = self
            .repo
            .revparse_single(&branch_ref)?
            .peel_to_commit()
            .context(format!("failed to resolve branch '{branch}'"))?;

        let main_tree = main_commit.tree()?;
        let branch_tree = branch_commit.tree()?;

        let diff = self
            .repo
            .diff_tree_to_tree(Some(&main_tree), Some(&branch_tree), None)?;

        let mut patch = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            // Prefix context/add/remove lines with their origin character
            if origin == '+' || origin == '-' || origin == ' ' {
                patch.push(origin);
            }
            if let Ok(content) = std::str::from_utf8(line.content()) {
                patch.push_str(content);
            }
            true
        })?;

        Ok(patch)
    }

    /// Merge the named branch into the default branch.
    pub fn merge_branch_to_main(&self, branch: &str) -> Result<String> {
        let main = self.default_branch()?;

        // Get the commit to merge
        let branch_ref = format!("refs/heads/{branch}");
        let branch_commit = self.repo.revparse_single(&branch_ref)?.peel_to_commit()?;
        let annotated = self.repo.find_annotated_commit(branch_commit.id())?;

        // Checkout main
        self.checkout(&main)?;

        // Merge
        self.repo
            .merge(&[&annotated], Some(&mut MergeOptions::new()), None)?;

        // Commit the merge
        let sig = self.signature()?;
        let head_commit = self.repo.head()?.peel_to_commit()?;
        let tree_id = self.repo.index()?.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;

        let merge_msg = format!("Merge branch '{branch}' into {main}");
        let oid = self.repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            &merge_msg,
            &tree,
            &[&head_commit, &branch_commit],
        )?;

        // Cleanup merge state
        self.repo.cleanup_state()?;

        Ok(oid.to_string())
    }

    /// Create a git worktree for the given branch, returning an isolated
    /// working directory. The worktree is placed under `base_dir/<branch_slug>`.
    ///
    /// The returned [`Worktree`](crate::worktree::Worktree) auto-cleans on drop.
    pub fn create_worktree(
        &self,
        branch: &str,
        base_dir: &Path,
    ) -> Result<crate::worktree::Worktree> {
        let repo_path = self
            .repo
            .workdir()
            .context("bare repository cannot create worktrees")?;
        crate::worktree::Worktree::create(repo_path, branch, base_dir)
    }

    /// Detect the default branch (main or master).
    fn default_branch(&self) -> Result<String> {
        if self.repo.find_branch("main", BranchType::Local).is_ok() {
            Ok("main".to_string())
        } else {
            Ok("master".to_string())
        }
    }

    /// Get or create a signature for commits.
    fn signature(&self) -> Result<Signature<'_>> {
        self.repo
            .signature()
            .or_else(|_| Signature::now("automator", "automator@pulseengine.dev"))
            .context("failed to create git signature")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a temporary git repo with an initial commit.
    ///
    /// Strips environment variables that may leak from the outer repo
    /// (e.g. `GIT_DIR`, `GIT_INDEX_FILE`) so the fresh repo is fully
    /// isolated.
    fn git_in(dir: &Path, args: &[&str]) {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
    }

    fn init_test_repo() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git_in(p, &["init", "-b", "main"]);
        git_in(p, &["config", "user.email", "test@test.com"]);
        git_in(p, &["config", "user.name", "Test"]);
        git_in(p, &["config", "commit.gpgsign", "false"]);
        git_in(p, &["commit", "--allow-empty", "-m", "initial"]);
        let git = GitRepo::open(p).unwrap();
        (dir, git)
    }

    #[test]
    fn create_worktree_via_git_repo() {
        let (repo_dir, git) = init_test_repo();
        let _ = repo_dir; // keep alive

        // Create a branch first
        git.create_branch("feature-wt").unwrap();
        // Switch back to main so the branch is available
        git.checkout("main").unwrap();

        let base = tempfile::tempdir().unwrap();
        let wt = git.create_worktree("feature-wt", base.path()).unwrap();

        assert!(wt.path.exists());
        assert!(wt.path.join(".git").exists());

        // Cleanup is automatic via Drop
        let path = wt.path.clone();
        drop(wt);
        assert!(!path.exists());
    }

    #[test]
    fn create_branch_and_get_sha() {
        let (_dir, git) = init_test_repo();
        let sha = git.head_sha().unwrap();
        assert!(!sha.is_empty());

        git.create_branch("test-branch").unwrap();
        let branch = git.current_branch().unwrap();
        assert_eq!(branch, "test-branch");
    }
}
