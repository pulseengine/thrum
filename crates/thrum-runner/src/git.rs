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

    /// Merge the current branch into the default branch.
    pub fn merge_to_main(&self) -> Result<String> {
        let current = self.current_branch()?;
        let main = self.default_branch()?;

        // Get the commit to merge
        let branch_ref = format!("refs/heads/{current}");
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

        let merge_msg = format!("Merge branch '{current}' into {main}");
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
