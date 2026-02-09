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
