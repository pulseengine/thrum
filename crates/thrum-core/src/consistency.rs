use crate::task::RepoName;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Version drift and configuration consistency across repos.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyReport {
    pub wasmparser_versions: HashMap<String, String>,
    pub z3_versions: HashMap<String, Z3Config>,
    pub rules_rust_versions: HashMap<String, String>,
    pub rust_editions: HashMap<String, String>,
    pub issues: Vec<ConsistencyIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Z3Config {
    pub crate_version: Option<String>,
    pub bazel_version: Option<String>,
    pub is_forked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConsistencyIssue {
    VersionDrift {
        dep: String,
        versions: HashMap<String, String>,
    },
    UnpinnedDependency {
        repo: String,
        dep: String,
        detail: String,
    },
    ProofToolchainMismatch {
        repos: Vec<String>,
        detail: String,
    },
    DuplicatedDefinition {
        name: String,
        locations: Vec<(String, String)>,
    },
}

impl std::fmt::Display for ConsistencyIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsistencyIssue::VersionDrift { dep, versions } => {
                write!(f, "Version drift for '{dep}': ")?;
                for (repo, ver) in versions {
                    write!(f, "{repo}={ver} ")?;
                }
                Ok(())
            }
            ConsistencyIssue::UnpinnedDependency { repo, dep, detail } => {
                write!(f, "Unpinned dep '{dep}' in {repo}: {detail}")
            }
            ConsistencyIssue::ProofToolchainMismatch { repos, detail } => {
                write!(f, "Proof toolchain mismatch across {repos:?}: {detail}")
            }
            ConsistencyIssue::DuplicatedDefinition { name, locations } => {
                write!(f, "Duplicated definition '{name}' at: ")?;
                for (repo, path) in locations {
                    write!(f, "{repo}:{path} ")?;
                }
                Ok(())
            }
        }
    }
}

/// Check consistency across all repos by parsing their Cargo.toml files.
pub fn check_consistency(
    repo_paths: &HashMap<RepoName, &Path>,
) -> anyhow::Result<ConsistencyReport> {
    let mut wasmparser_versions = HashMap::new();
    let mut z3_versions = HashMap::new();
    let mut rules_rust_versions = HashMap::new();
    let mut rust_editions = HashMap::new();
    let mut issues = Vec::new();

    for (name, path) in repo_paths {
        let repo_label = name.to_string();
        let cargo_path = path.join("Cargo.toml");

        if !cargo_path.exists() {
            tracing::warn!(?cargo_path, "Cargo.toml not found, skipping");
            continue;
        }

        let manifest = cargo_toml::Manifest::from_path(&cargo_path)?;

        // Extract edition
        if let Some(pkg) = &manifest.package {
            let edition = &pkg.edition;
            // Inheritable<Edition> — get the value if set explicitly
            if let Ok(ed) = edition.get() {
                let edition_str = format!("{ed:?}");
                rust_editions.insert(repo_label.clone(), edition_str);
            }
        }

        // Check workspace dependencies if present
        let deps = if let Some(ref ws) = manifest.workspace {
            ws.dependencies.clone()
        } else {
            manifest.dependencies.clone()
        };

        for (dep_name, dep) in &deps {
            let version_str = match dep {
                cargo_toml::Dependency::Simple(v) => v.clone(),
                cargo_toml::Dependency::Inherited(_) => continue,
                cargo_toml::Dependency::Detailed(d) => d.version.clone().unwrap_or_default(),
            };

            match dep_name.as_str() {
                "wasmparser" => {
                    wasmparser_versions.insert(repo_label.clone(), version_str);
                }
                "z3" | "z3-sys" => {
                    let config =
                        z3_versions
                            .entry(repo_label.clone())
                            .or_insert_with(|| Z3Config {
                                crate_version: None,
                                bazel_version: None,
                                is_forked: false,
                            });
                    config.crate_version = Some(version_str);
                }
                "rules_rust" => {
                    rules_rust_versions.insert(repo_label.clone(), version_str);
                }
                _ => {}
            }
        }
    }

    // Detect version drift for wasmparser
    if wasmparser_versions
        .values()
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1
    {
        issues.push(ConsistencyIssue::VersionDrift {
            dep: "wasmparser".into(),
            versions: wasmparser_versions.clone(),
        });
    }

    // Detect edition drift
    if rust_editions
        .values()
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1
    {
        issues.push(ConsistencyIssue::VersionDrift {
            dep: "rust-edition".into(),
            versions: rust_editions.clone(),
        });
    }

    Ok(ConsistencyReport {
        wasmparser_versions,
        z3_versions,
        rules_rust_versions,
        rust_editions,
        issues,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_display() {
        let issue = ConsistencyIssue::VersionDrift {
            dep: "wasmparser".into(),
            versions: HashMap::from([
                ("loom".into(), "0.241".into()),
                ("synth".into(), "0.219".into()),
            ]),
        };
        let s = issue.to_string();
        assert!(s.contains("wasmparser"));
        assert!(s.contains("0.241"));
    }
}
