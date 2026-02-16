//! Agent session checkpoint for resumable pipeline runs.
//!
//! When an agent crashes or times out mid-pipeline, checkpoints allow
//! resumption from the last successful gate instead of restarting from
//! scratch. Inspired by the Gas Town GUPP autonomous resumption pattern.
//!
//! Checkpoints are saved after each gate pass and capture enough state
//! to skip already-completed gates on resume.

use crate::task::{GateReport, RepoName, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Identifies a checkpoint uniquely by task ID.
/// One active checkpoint per task at most.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub String);

impl CheckpointId {
    /// Create a checkpoint ID for a task.
    pub fn for_task(task_id: &TaskId) -> Self {
        Self(format!("checkpoint:{}", task_id.0))
    }
}

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Which pipeline phase was last completed successfully.
///
/// This determines where resumption picks up. The pipeline stages
/// are ordered: Implementation → Gate1 → Review → Gate2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletedPhase {
    /// Implementation finished, gate checks not yet started.
    Implementation,
    /// Gate 1 (Quality) passed.
    Gate1Passed,
    /// Review completed (reviewer output captured).
    ReviewCompleted,
    /// Gate 2 (Proof) passed. Task is ready for AwaitingApproval.
    Gate2Passed,
}

impl CompletedPhase {
    /// Short label for display.
    pub fn label(&self) -> &'static str {
        match self {
            CompletedPhase::Implementation => "implementation",
            CompletedPhase::Gate1Passed => "gate1-passed",
            CompletedPhase::ReviewCompleted => "review-completed",
            CompletedPhase::Gate2Passed => "gate2-passed",
        }
    }
}

impl std::fmt::Display for CompletedPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// A checkpoint capturing the state of a task's pipeline progress.
///
/// Saved after each successful gate pass so that if the agent crashes
/// or times out, the pipeline can resume from the last checkpoint
/// instead of restarting the full cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique checkpoint identifier (derived from task ID).
    pub id: CheckpointId,
    /// The task this checkpoint belongs to.
    pub task_id: TaskId,
    /// Target repository.
    pub repo: RepoName,
    /// The git branch for this task's implementation.
    pub branch: String,
    /// Last successfully completed pipeline phase.
    pub completed_phase: CompletedPhase,
    /// Gate 1 report, if Gate 1 has passed.
    pub gate1_report: Option<GateReport>,
    /// Reviewer output, if review has completed.
    pub reviewer_output: Option<String>,
    /// Gate 2 report, if Gate 2 has passed.
    pub gate2_report: Option<GateReport>,
    /// Memory entry IDs that were relevant at checkpoint time.
    pub memory_ids: Vec<String>,
    /// When this checkpoint was created.
    pub created_at: DateTime<Utc>,
    /// When this checkpoint was last updated.
    pub updated_at: DateTime<Utc>,
}

impl Checkpoint {
    /// Create a new checkpoint after implementation is complete.
    pub fn after_implementation(task_id: TaskId, repo: RepoName, branch: String) -> Self {
        let now = Utc::now();
        Self {
            id: CheckpointId::for_task(&task_id),
            task_id,
            repo,
            branch,
            completed_phase: CompletedPhase::Implementation,
            gate1_report: None,
            reviewer_output: None,
            gate2_report: None,
            memory_ids: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Advance checkpoint to Gate 1 passed.
    pub fn advance_to_gate1(&mut self, report: GateReport) {
        self.completed_phase = CompletedPhase::Gate1Passed;
        self.gate1_report = Some(report);
        self.updated_at = Utc::now();
    }

    /// Advance checkpoint to review completed.
    pub fn advance_to_review(&mut self, reviewer_output: String) {
        self.completed_phase = CompletedPhase::ReviewCompleted;
        self.reviewer_output = Some(reviewer_output);
        self.updated_at = Utc::now();
    }

    /// Advance checkpoint to Gate 2 passed.
    pub fn advance_to_gate2(&mut self, report: GateReport) {
        self.completed_phase = CompletedPhase::Gate2Passed;
        self.gate2_report = Some(report);
        self.updated_at = Utc::now();
    }

    /// Whether Gate 1 has been passed in this checkpoint.
    pub fn gate1_passed(&self) -> bool {
        matches!(
            self.completed_phase,
            CompletedPhase::Gate1Passed
                | CompletedPhase::ReviewCompleted
                | CompletedPhase::Gate2Passed
        )
    }

    /// Whether review has been completed in this checkpoint.
    pub fn review_completed(&self) -> bool {
        matches!(
            self.completed_phase,
            CompletedPhase::ReviewCompleted | CompletedPhase::Gate2Passed
        )
    }

    /// Whether Gate 2 has been passed in this checkpoint.
    pub fn gate2_passed(&self) -> bool {
        matches!(self.completed_phase, CompletedPhase::Gate2Passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{CheckResult, GateLevel};

    fn sample_gate_report(level: GateLevel) -> GateReport {
        GateReport {
            level,
            checks: vec![CheckResult {
                name: "test".into(),
                passed: true,
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
            }],
            passed: true,
            duration_secs: 1.5,
        }
    }

    #[test]
    fn checkpoint_lifecycle() {
        let mut cp = Checkpoint::after_implementation(
            TaskId(42),
            RepoName::new("loom"),
            "auto/TASK-0042/loom/test".into(),
        );
        assert_eq!(cp.completed_phase, CompletedPhase::Implementation);
        assert!(!cp.gate1_passed());
        assert!(!cp.review_completed());
        assert!(!cp.gate2_passed());

        cp.advance_to_gate1(sample_gate_report(GateLevel::Quality));
        assert!(cp.gate1_passed());
        assert!(!cp.review_completed());
        assert!(cp.gate1_report.is_some());

        cp.advance_to_review("looks good".into());
        assert!(cp.review_completed());
        assert_eq!(cp.reviewer_output.as_deref(), Some("looks good"));

        cp.advance_to_gate2(sample_gate_report(GateLevel::Proof));
        assert!(cp.gate2_passed());
        assert!(cp.gate2_report.is_some());
    }

    #[test]
    fn checkpoint_id_deterministic() {
        let id1 = CheckpointId::for_task(&TaskId(1));
        let id2 = CheckpointId::for_task(&TaskId(1));
        assert_eq!(id1, id2);
    }

    #[test]
    fn checkpoint_id_unique_per_task() {
        let id1 = CheckpointId::for_task(&TaskId(1));
        let id2 = CheckpointId::for_task(&TaskId(2));
        assert_ne!(id1, id2);
    }

    #[test]
    fn completed_phase_labels() {
        assert_eq!(CompletedPhase::Implementation.label(), "implementation");
        assert_eq!(CompletedPhase::Gate1Passed.label(), "gate1-passed");
        assert_eq!(CompletedPhase::ReviewCompleted.label(), "review-completed");
        assert_eq!(CompletedPhase::Gate2Passed.label(), "gate2-passed");
    }

    #[test]
    fn checkpoint_serialization_roundtrip() {
        let mut cp = Checkpoint::after_implementation(
            TaskId(7),
            RepoName::new("synth"),
            "auto/TASK-0007/synth/feature".into(),
        );
        cp.advance_to_gate1(sample_gate_report(GateLevel::Quality));
        cp.memory_ids = vec!["mem-1".into(), "mem-2".into()];

        let json = serde_json::to_string(&cp).unwrap();
        let parsed: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, TaskId(7));
        assert!(parsed.gate1_passed());
        assert_eq!(parsed.memory_ids.len(), 2);
    }
}
