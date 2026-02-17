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

    /// Create a branch ref without checking it out, or update it to HEAD if
    /// it already exists.
    ///
    /// Used when creating worktrees: the branch must exist as a ref but must
    /// NOT be checked out in the main working directory, otherwise
    /// `git worktree add` will fail with "already used by worktree".
    ///
    /// Uses `force=true` so that existing branches (e.g. from a previous run)
    /// are updated to the current HEAD instead of silently keeping a stale
    /// commit pointer.
    pub fn create_branch_detached(&self, name: &str) -> Result<()> {
        let head_commit = self.repo.head()?.peel_to_commit()?;
        self.repo.branch(name, &head_commit, true)?;
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

    /// Salvage uncommitted changes by staging and committing them as WIP.
    ///
    /// Returns `Ok(true)` if a WIP commit was created, `Ok(false)` if the
    /// worktree was clean (nothing to salvage). Errors are non-fatal — the
    /// caller should log and continue.
    ///
    /// This is used to preserve partial agent work when an invocation times
    /// out or fails before the agent can commit. The WIP commit stays on
    /// the branch so the next retry has the partial progress.
    pub fn salvage_uncommitted(&self, message: &str) -> Result<bool> {
        // Remove stale index.lock left by killed agent processes.
        if let Some(workdir) = self.repo.workdir() {
            let lock = workdir.join(".git/index.lock");
            if lock.exists() {
                tracing::warn!(path = %lock.display(), "removing stale index.lock");
                let _ = std::fs::remove_file(&lock);
            }
        }

        // Check if there's anything to salvage.
        let statuses = self.repo.statuses(None)?;
        if statuses.is_empty() {
            return Ok(false);
        }

        // Stage all changes (respects .gitignore).
        let mut index = self.repo.index()?;
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = self.repo.find_tree(tree_oid)?;

        // Commit.
        let sig = self.signature()?;
        let head = self.repo.head()?.peel_to_commit()?;
        self.repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&head])?;

        Ok(true)
    }

    /// Fetch from a remote using the git CLI (for SSH agent forwarding).
    pub fn fetch_remote(&self, remote: &str) -> Result<()> {
        let workdir = self
            .repo
            .workdir()
            .context("bare repository cannot fetch")?;
        let output = std::process::Command::new("git")
            .args(["fetch", remote])
            .current_dir(workdir)
            .output()
            .context("failed to run `git fetch`")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git fetch {remote} failed: {stderr}");
        }
        Ok(())
    }

    /// Fast-forward or rebase local main to match remote main.
    ///
    /// Returns `(old_sha, new_sha)` if main was updated, or `None` if
    /// already up-to-date.
    pub fn sync_main_to_remote(&self, remote: &str) -> Result<Option<(String, String)>> {
        let main = self.default_branch()?;
        let workdir = self.repo.workdir().context("bare repository cannot sync")?;

        let old_sha = {
            let main_ref = format!("refs/heads/{main}");
            self.repo.revparse_single(&main_ref)?.id().to_string()
        };

        // Use git CLI for the merge/rebase since it handles remote refs better.
        let output = std::process::Command::new("git")
            .args(["rebase", &format!("{remote}/{main}"), &main])
            .current_dir(workdir)
            .output()
            .context("failed to rebase main onto remote")?;

        if !output.status.success() {
            // Abort the failed rebase
            let _ = std::process::Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(workdir)
                .output();
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to sync main to {remote}: {stderr}");
        }

        let new_sha = {
            let main_ref = format!("refs/heads/{main}");
            // Re-open to see updated refs
            let repo = Repository::open(workdir)?;
            repo.revparse_single(&main_ref)?.id().to_string()
        };

        if old_sha == new_sha {
            Ok(None)
        } else {
            Ok(Some((old_sha, new_sha)))
        }
    }

    /// Rebase a branch onto the default branch (main).
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` if there were conflicts
    /// (the rebase is aborted in that case).
    pub fn rebase_branch_onto_main(&self, branch: &str) -> Result<bool> {
        let main = self.default_branch()?;
        let workdir = self
            .repo
            .workdir()
            .context("bare repository cannot rebase")?;

        let output = std::process::Command::new("git")
            .args(["rebase", &main, branch])
            .current_dir(workdir)
            .output()
            .context("failed to rebase branch")?;

        if !output.status.success() {
            // Abort the failed rebase
            let _ = std::process::Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(workdir)
                .output();
            return Ok(false);
        }
        Ok(true)
    }

    /// List all task branches matching the `auto/*` naming convention.
    pub fn list_task_branches(&self) -> Result<Vec<String>> {
        let mut branches = Vec::new();
        for branch in self.repo.branches(Some(BranchType::Local))? {
            let (branch, _) = branch?;
            if let Some(name) = branch.name()?
                && name.starts_with("auto/")
            {
                branches.push(name.to_string());
            }
        }
        Ok(branches)
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

    #[test]
    fn salvage_uncommitted_creates_wip_commit() {
        let (dir, git) = init_test_repo();
        let initial_sha = git.head_sha().unwrap();

        // Write an uncommitted file
        std::fs::write(dir.path().join("partial.rs"), "fn wip() {}").unwrap();
        assert!(!git.is_clean().unwrap());

        // Salvage it
        let committed = git.salvage_uncommitted("WIP: timed out").unwrap();
        assert!(committed);

        // Worktree should now be clean
        assert!(git.is_clean().unwrap());

        // HEAD should have moved
        let new_sha = git.head_sha().unwrap();
        assert_ne!(initial_sha, new_sha);
    }

    #[test]
    fn salvage_uncommitted_noop_on_clean_worktree() {
        let (_dir, git) = init_test_repo();
        let committed = git.salvage_uncommitted("WIP: nothing here").unwrap();
        assert!(!committed);
    }

    #[test]
    fn salvage_uncommitted_removes_stale_index_lock() {
        let (dir, git) = init_test_repo();
        let lock_path = dir.path().join(".git/index.lock");
        std::fs::write(&lock_path, "stale").unwrap();
        assert!(lock_path.exists());

        // Write a file so there's something to commit
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();

        let committed = git.salvage_uncommitted("WIP: after lock cleanup").unwrap();
        assert!(committed);
        assert!(!lock_path.exists());
    }

    /// Create a bare "remote" and a clone of it for testing fetch/sync.
    fn init_remote_and_clone() -> (tempfile::TempDir, tempfile::TempDir, GitRepo) {
        // Create the "remote" repo
        let remote_dir = tempfile::tempdir().unwrap();
        let rp = remote_dir.path();
        git_in(rp, &["init", "-b", "main"]);
        git_in(rp, &["config", "user.email", "test@test.com"]);
        git_in(rp, &["config", "user.name", "Test"]);
        git_in(rp, &["config", "commit.gpgsign", "false"]);
        std::fs::write(rp.join("README.md"), "# Test").unwrap();
        git_in(rp, &["add", "."]);
        git_in(rp, &["commit", "-m", "initial"]);

        // Clone it
        let clone_dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args([
                "clone",
                rp.to_str().unwrap(),
                clone_dir.path().to_str().unwrap(),
            ])
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        git_in(clone_dir.path(), &["config", "user.email", "test@test.com"]);
        git_in(clone_dir.path(), &["config", "user.name", "Test"]);
        git_in(clone_dir.path(), &["config", "commit.gpgsign", "false"]);

        let git = GitRepo::open(clone_dir.path()).unwrap();
        (remote_dir, clone_dir, git)
    }

    #[test]
    fn fetch_remote_succeeds() {
        let (_remote_dir, _clone_dir, git) = init_remote_and_clone();
        git.fetch_remote("origin").unwrap();
    }

    #[test]
    fn sync_main_to_remote_detects_no_changes() {
        let (_remote_dir, _clone_dir, git) = init_remote_and_clone();
        git.fetch_remote("origin").unwrap();
        let result = git.sync_main_to_remote("origin").unwrap();
        assert!(result.is_none(), "should be up-to-date");
    }

    #[test]
    fn sync_main_to_remote_detects_new_commits() {
        let (remote_dir, _clone_dir, git) = init_remote_and_clone();

        // Push a new commit to the remote
        std::fs::write(remote_dir.path().join("new.txt"), "new content").unwrap();
        git_in(remote_dir.path(), &["add", "."]);
        git_in(remote_dir.path(), &["commit", "-m", "remote advance"]);

        git.fetch_remote("origin").unwrap();
        let result = git.sync_main_to_remote("origin").unwrap();
        assert!(result.is_some(), "should detect new commits");
        let (old_sha, new_sha) = result.unwrap();
        assert_ne!(old_sha, new_sha);
    }

    #[test]
    fn list_task_branches_filters_auto_prefix() {
        let (_dir, git) = init_test_repo();
        git.create_branch_detached("auto/TASK-0001/repo/feature")
            .unwrap();
        git.create_branch_detached("auto/TASK-0002/repo/other")
            .unwrap();
        git.create_branch_detached("feature/unrelated").unwrap();

        let branches = git.list_task_branches().unwrap();
        assert_eq!(branches.len(), 2);
        assert!(branches.iter().all(|b| b.starts_with("auto/")));
    }

    #[test]
    fn rebase_branch_onto_main_succeeds_clean() {
        let (dir, git) = init_test_repo();
        // Create a branch with a commit
        git.create_branch("auto/TASK-0001/repo/feat").unwrap();
        std::fs::write(dir.path().join("branch_file.txt"), "branch work").unwrap();
        git_in(dir.path(), &["add", "."]);
        git_in(dir.path(), &["commit", "-m", "branch commit"]);

        // Go back to main and add a commit there too
        git.checkout("main").unwrap();
        std::fs::write(dir.path().join("main_file.txt"), "main work").unwrap();
        git_in(dir.path(), &["add", "."]);
        git_in(dir.path(), &["commit", "-m", "main advance"]);

        // Rebase should succeed
        let result = git
            .rebase_branch_onto_main("auto/TASK-0001/repo/feat")
            .unwrap();
        assert!(result, "clean rebase should succeed");
    }

    #[test]
    fn create_branch_detached_updates_existing_branch_to_head() {
        let (dir, git) = init_test_repo();

        // Create a detached branch at the initial commit.
        git.create_branch_detached("feature-x").unwrap();
        let initial_sha = git.head_sha().unwrap();

        // Advance HEAD with a new commit on main.
        std::fs::write(dir.path().join("new.txt"), "content").unwrap();
        git_in(dir.path(), &["add", "."]);
        git_in(dir.path(), &["commit", "-m", "second"]);
        let advanced_sha = git.head_sha().unwrap();
        assert_ne!(initial_sha, advanced_sha);

        // Calling create_branch_detached again must update the branch to the
        // new HEAD, not leave it pointing at the old commit.
        git.create_branch_detached("feature-x").unwrap();

        let branch = git
            .repo
            .find_branch("feature-x", BranchType::Local)
            .unwrap();
        let branch_sha = branch.get().target().unwrap().to_string();
        assert_eq!(branch_sha, advanced_sha);
    }
}
