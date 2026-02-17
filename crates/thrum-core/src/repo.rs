use crate::task::{AsilLevel, RepoName};
use serde::Deserialize;
use std::path::PathBuf;

/// Configuration for a managed repository.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoConfig {
    pub name: RepoName,
    pub path: PathBuf,
    pub build_cmd: String,
    pub test_cmd: String,
    pub lint_cmd: String,
    pub fmt_cmd: String,
    /// Z3 verification command, if applicable.
    pub verify_cmd: Option<String>,
    /// Rocq/Coq proof build command, if applicable.
    pub proofs_cmd: Option<String>,
    /// Path to the repo's CLAUDE.md for agent prompt embedding.
    pub claude_md: Option<PathBuf>,
    /// Functional safety target for this tool.
    pub safety_target: Option<AsilLevel>,
    /// CI integration configuration (opt-in).
    #[serde(default)]
    pub ci: Option<CIConfig>,
}

/// CI integration configuration for a repository.
///
/// When present, the post-approval pipeline will push the branch,
/// create a PR, and poll CI status instead of merging locally.
#[derive(Debug, Clone, Deserialize)]
pub struct CIConfig {
    /// Whether CI integration is enabled.
    #[serde(default = "default_ci_enabled")]
    pub enabled: bool,
    /// Polling interval in seconds (default: 60).
    #[serde(default = "default_ci_poll_interval")]
    pub poll_interval_secs: u64,
    /// Maximum number of ci_fixer retries before escalating (default: 3).
    #[serde(default = "default_max_ci_retries")]
    pub max_ci_retries: u32,
    /// Whether to auto-merge on green CI (default: true).
    #[serde(default = "default_auto_merge")]
    pub auto_merge: bool,
    /// Merge strategy: "squash", "merge", "rebase" (default: "squash").
    #[serde(default = "default_merge_strategy")]
    pub merge_strategy: String,
}

fn default_ci_enabled() -> bool {
    true
}

fn default_ci_poll_interval() -> u64 {
    60
}

fn default_max_ci_retries() -> u32 {
    3
}

fn default_auto_merge() -> bool {
    true
}

fn default_merge_strategy() -> String {
    "squash".into()
}

impl Default for CIConfig {
    fn default() -> Self {
        Self {
            enabled: default_ci_enabled(),
            poll_interval_secs: default_ci_poll_interval(),
            max_ci_retries: default_max_ci_retries(),
            auto_merge: default_auto_merge(),
            merge_strategy: default_merge_strategy(),
        }
    }
}

impl RepoConfig {
    /// Return a clone with `path` overridden to `work_dir`.
    ///
    /// Used for worktree-based isolation: gate checks, AI requests, and git
    /// operations all derive their working directory from `repo_config.path`,
    /// so swapping it is sufficient to redirect everything into a worktree.
    pub fn with_work_dir(&self, work_dir: PathBuf) -> Self {
        let mut cloned = self.clone();
        cloned.path = work_dir;
        cloned
    }
}

/// Top-level repos configuration (parsed from repos.toml).
#[derive(Debug, Clone, Deserialize)]
pub struct ReposConfig {
    pub repo: Vec<RepoConfig>,
}

impl ReposConfig {
    /// Load from a TOML file.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: ReposConfig = toml::from_str(&content)?;
        Ok(config)
    }

    /// Find config for a specific repo.
    pub fn get(&self, name: &RepoName) -> Option<&RepoConfig> {
        self.repo.iter().find(|r| &r.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_repo_config() -> RepoConfig {
        RepoConfig {
            name: RepoName::new("test"),
            path: PathBuf::from("/original/path"),
            build_cmd: "cargo build".into(),
            test_cmd: "cargo test".into(),
            lint_cmd: "cargo clippy".into(),
            fmt_cmd: "cargo fmt --check".into(),
            verify_cmd: None,
            proofs_cmd: None,
            claude_md: None,
            safety_target: None,
            ci: None,
        }
    }

    #[test]
    fn with_work_dir_overrides_path_only() {
        let config = test_repo_config();
        let overridden = config.with_work_dir(PathBuf::from("/worktree/path"));

        assert_eq!(overridden.path, PathBuf::from("/worktree/path"));
        // Everything else should be preserved
        assert_eq!(overridden.name, config.name);
        assert_eq!(overridden.build_cmd, config.build_cmd);
        assert_eq!(overridden.test_cmd, config.test_cmd);
        assert_eq!(overridden.lint_cmd, config.lint_cmd);
        assert_eq!(overridden.fmt_cmd, config.fmt_cmd);
    }
}
