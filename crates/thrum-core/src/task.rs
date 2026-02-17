use crate::spec::Spec;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Maximum number of automatic retries before requiring human intervention.
pub const MAX_RETRIES: u32 = 10;

/// Unique task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub i64);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TASK-{:04}", self.0)
    }
}

/// Which repository a task targets.
///
/// Fully dynamic — any repo name is valid. Stored and compared in lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct RepoName(pub String);

impl<'de> Deserialize<'de> for RepoName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(RepoName::new(s))
    }
}

impl RepoName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into().to_lowercase())
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for RepoName {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(RepoName::new(s))
    }
}

// Re-export from safety module — single source of truth.
pub use crate::safety::AsilLevel;

/// Gate report summarizing verification results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub level: GateLevel,
    pub checks: Vec<CheckResult>,
    pub passed: bool,
    pub duration_secs: f64,
}

/// Which verification gate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GateLevel {
    /// Gate 1: cargo fmt + clippy + test
    Quality,
    /// Gate 2: Z3 + Rocq proofs
    Proof,
    /// Gate 3: meld -> loom -> synth integration pipeline
    Integration,
}

impl fmt::Display for GateLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateLevel::Quality => write!(f, "Gate 1: Quality"),
            GateLevel::Proof => write!(f, "Gate 2: Proof"),
            GateLevel::Integration => write!(f, "Gate 3: Integration"),
        }
    }
}

/// Result of a single verification check within a gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Summary shown at human review checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub diff_summary: String,
    pub reviewer_output: String,
    pub gate1_report: GateReport,
    pub gate2_report: Option<GateReport>,
}

/// Task status as a state machine.
///
/// Transitions:
///   Pending -> Implementing -> Gate1Failed | Reviewing
///   Reviewing -> Gate2Failed | AwaitingApproval
///   AwaitingApproval -> Approved | Rejected
///   Approved -> Integrating -> Gate3Failed | Merged
///   *Failed -> Implementing (retry)
///   Rejected -> Implementing (with feedback)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Claimed {
        agent_id: String,
        claimed_at: DateTime<Utc>,
    },
    Implementing {
        branch: String,
        started_at: DateTime<Utc>,
    },
    Gate1Failed {
        report: GateReport,
    },
    Reviewing {
        reviewer_output: String,
    },
    Gate2Failed {
        report: GateReport,
    },
    AwaitingApproval {
        summary: CheckpointSummary,
    },
    Approved,
    Integrating,
    Gate3Failed {
        report: GateReport,
    },
    Merged {
        commit_sha: String,
    },
    Rejected {
        feedback: String,
    },
}

impl TaskStatus {
    /// Short label for display.
    pub fn label(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Claimed { .. } => "claimed",
            TaskStatus::Implementing { .. } => "implementing",
            TaskStatus::Gate1Failed { .. } => "gate1-failed",
            TaskStatus::Reviewing { .. } => "reviewing",
            TaskStatus::Gate2Failed { .. } => "gate2-failed",
            TaskStatus::AwaitingApproval { .. } => "awaiting-approval",
            TaskStatus::Approved => "approved",
            TaskStatus::Integrating => "integrating",
            TaskStatus::Gate3Failed { .. } => "gate3-failed",
            TaskStatus::Merged { .. } => "merged",
            TaskStatus::Rejected { .. } => "rejected",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Merged { .. })
    }

    pub fn needs_human(&self) -> bool {
        matches!(self, TaskStatus::AwaitingApproval { .. })
    }

    /// Whether this task has a reviewable diff (in Reviewing or AwaitingApproval).
    pub fn is_reviewable(&self) -> bool {
        matches!(
            self,
            TaskStatus::Reviewing { .. } | TaskStatus::AwaitingApproval { .. }
        )
    }

    /// Whether this status is claimable as a new pending task.
    pub fn is_claimable_pending(&self) -> bool {
        matches!(self, TaskStatus::Pending)
    }

    /// Whether this status is claimable as a retryable failure.
    pub fn is_claimable_retry(&self) -> bool {
        matches!(
            self,
            TaskStatus::Gate1Failed { .. }
                | TaskStatus::Gate2Failed { .. }
                | TaskStatus::Rejected { .. }
        )
    }

    /// Whether this status is claimable as an approved task.
    pub fn is_claimable_approved(&self) -> bool {
        matches!(self, TaskStatus::Approved)
    }
}

/// A task in the autonomous development pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    /// Functional safety requirement ID, e.g. "REQ-LOOM-042"
    pub requirement_id: Option<String>,
    pub repo: RepoName,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    pub status: TaskStatus,
    pub safety_classification: Option<AsilLevel>,
    /// A2A context ID for grouping related tasks.
    #[serde(default)]
    pub context_id: Option<String>,
    /// Structured specification (SDD). If present, used instead of free-text description.
    #[serde(default)]
    pub spec: Option<Spec>,
    /// How many times this task has been retried after gate failure.
    #[serde(default)]
    pub retry_count: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    /// Create a new pending task.
    pub fn new(repo: RepoName, title: String, description: String) -> Self {
        let now = Utc::now();
        Self {
            id: TaskId(0), // assigned by DB
            requirement_id: None,
            repo,
            title,
            description,
            acceptance_criteria: Vec::new(),
            status: TaskStatus::Pending,
            safety_classification: None,
            context_id: None,
            spec: None,
            retry_count: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// Whether this task can be retried (under MAX_RETRIES).
    pub fn can_retry(&self) -> bool {
        self.retry_count < MAX_RETRIES
    }

    /// Branch name for this task's implementation.
    pub fn branch_name(&self) -> String {
        let slug: String = self
            .title
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        // Collapse repeated dashes
        let mut result = String::new();
        let mut prev_dash = false;
        for c in slug.chars() {
            if c == '-' {
                if !prev_dash {
                    result.push(c);
                }
                prev_dash = true;
            } else {
                result.push(c);
                prev_dash = false;
            }
        }
        format!("auto/{}/{}/{}", self.id, self.repo, result)
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Strategy to generate arbitrary RepoName values.
    fn arb_repo_name() -> impl Strategy<Value = RepoName> {
        "[a-z][a-z0-9_-]{0,30}".prop_map(RepoName::new)
    }

    /// Strategy to generate arbitrary Task values.
    fn arb_task() -> impl Strategy<Value = Task> {
        (arb_repo_name(), ".*", ".*")
            .prop_map(|(repo, title, desc)| Task::new(repo, title.to_string(), desc.to_string()))
    }

    proptest! {
        /// RepoName roundtrips through Display + FromStr.
        #[test]
        fn repo_name_display_fromstr_roundtrip(name in "[a-z][a-z0-9_-]{0,30}") {
            let repo = RepoName::new(&name);
            let displayed = repo.to_string();
            let parsed: RepoName = displayed.parse().unwrap();
            prop_assert_eq!(repo, parsed);
        }

        /// New tasks always start as Pending.
        #[test]
        fn new_task_always_pending(task in arb_task()) {
            prop_assert_eq!(task.status.label(), "pending");
            prop_assert_eq!(task.retry_count, 0);
        }

        /// retry_count >= MAX_RETRIES means can_retry() returns false.
        #[test]
        fn retry_count_bounded(retry_count in 0u32..20) {
            let mut task = Task::new(RepoName::new("test"), "t".into(), "d".into());
            task.retry_count = retry_count;
            if retry_count >= MAX_RETRIES {
                prop_assert!(!task.can_retry());
            }
        }

        /// Branch name is deterministic for the same task.
        #[test]
        fn branch_name_deterministic(task in arb_task()) {
            let b1 = task.branch_name();
            let b2 = task.branch_name();
            prop_assert_eq!(b1, b2);
        }

        /// Branch name contains the repo name.
        #[test]
        fn branch_name_contains_repo(task in arb_task()) {
            let branch = task.branch_name();
            prop_assert!(branch.contains(&task.repo.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_name_format() {
        let mut task = Task::new(
            RepoName::new("loom"),
            "Add i32.popcnt to ISLE pipeline".into(),
            "desc".into(),
        );
        task.id = TaskId(42);
        assert_eq!(
            task.branch_name(),
            "auto/TASK-0042/loom/add-i32-popcnt-to-isle-pipeline"
        );
    }

    #[test]
    fn status_labels() {
        assert_eq!(TaskStatus::Pending.label(), "pending");
        assert!(
            TaskStatus::Merged {
                commit_sha: "abc".into()
            }
            .is_terminal()
        );
        assert!(
            TaskStatus::AwaitingApproval {
                summary: CheckpointSummary {
                    diff_summary: String::new(),
                    reviewer_output: String::new(),
                    gate1_report: GateReport {
                        level: GateLevel::Quality,
                        checks: vec![],
                        passed: true,
                        duration_secs: 0.0,
                    },
                    gate2_report: None,
                }
            }
            .needs_human()
        );
    }

    #[test]
    fn repo_name_roundtrip() {
        assert_eq!("loom".parse::<RepoName>().unwrap(), RepoName::new("loom"));
        assert_eq!("Meld".parse::<RepoName>().unwrap(), RepoName::new("meld"));
        assert_eq!("SYNTH".parse::<RepoName>().unwrap(), RepoName::new("synth"));
        // Any name is valid now — fully dynamic
        assert_eq!(
            "custom-project".parse::<RepoName>().unwrap(),
            RepoName::new("custom-project")
        );
    }
}
