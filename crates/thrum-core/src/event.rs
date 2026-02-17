//! Pipeline event types for real-time observability.
//!
//! Events are emitted by the engine as tasks progress through the pipeline.
//! Consumers (TUI, SSE endpoint, JSONL logger) subscribe and render them.
//!
//! These are pure data types with no async runtime dependency — the
//! broadcast bus lives in `thrum-runner`.

use crate::agent::AgentId;
use crate::checkpoint::CompletedPhase;
use crate::coordination::{ConflictPolicy, FileConflict};
use crate::task::{GateLevel, RepoName, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A timestamped pipeline event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineEvent {
    pub timestamp: DateTime<Utc>,
    pub kind: EventKind,
}

impl PipelineEvent {
    pub fn new(kind: EventKind) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
        }
    }
}

/// The specific kind of pipeline event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    /// Task status changed.
    TaskStateChange {
        task_id: TaskId,
        repo: RepoName,
        from: String,
        to: String,
    },

    /// Agent spawned and started working.
    AgentStarted {
        agent_id: AgentId,
        task_id: TaskId,
        repo: RepoName,
    },

    /// A line of output from an agent subprocess.
    AgentOutput {
        agent_id: AgentId,
        task_id: TaskId,
        stream: OutputStream,
        line: String,
    },

    /// Agent finished execution.
    AgentFinished {
        agent_id: AgentId,
        task_id: TaskId,
        success: bool,
        elapsed_secs: f64,
    },

    /// Gate check started.
    GateStarted { task_id: TaskId, level: GateLevel },

    /// A line of output from a gate check subprocess.
    GateOutput {
        task_id: TaskId,
        level: GateLevel,
        check_name: String,
        stream: OutputStream,
        line: String,
    },

    /// Individual gate check completed.
    GateCheckFinished {
        task_id: TaskId,
        level: GateLevel,
        check_name: String,
        passed: bool,
    },

    /// Entire gate finished.
    GateFinished {
        task_id: TaskId,
        level: GateLevel,
        passed: bool,
        duration_secs: f64,
    },

    /// A file changed in a watched repo working directory.
    FileChanged {
        agent_id: AgentId,
        task_id: TaskId,
        path: std::path::PathBuf,
        kind: FileChangeKind,
    },

    /// Periodic diff statistics for an in-progress agent task.
    DiffUpdate {
        agent_id: AgentId,
        task_id: TaskId,
        files_changed: u32,
        insertions: u32,
        deletions: u32,
    },

    /// Engine-level log message (info, warn, error).
    EngineLog { level: LogLevel, message: String },

    /// Agent session checkpoint saved for resumable runs.
    CheckpointSaved {
        task_id: TaskId,
        repo: RepoName,
        phase: CompletedPhase,
    },

    /// Agent session continued from a previous invocation (timeout/failure recovery).
    SessionContinued {
        task_id: TaskId,
        repo: RepoName,
        session_id: String,
    },

    // -- Agent-to-agent coordination events --
    /// Two agents touched the same file — a file conflict was detected.
    FileConflictDetected {
        conflict: FileConflict,
        policy: ConflictPolicy,
    },

    /// An agent published a cross-agent notification.
    CrossAgentNotification {
        source: AgentId,
        source_task: TaskId,
        message: String,
        affected_files: Vec<PathBuf>,
    },

    /// An agent wrote to the shared memory store.
    SharedMemoryWrite {
        agent_id: AgentId,
        key: String,
        value: String,
    },

    /// Convergence detected: task keeps failing with the same error signature.
    TaskConvergenceDetected {
        task_id: TaskId,
        /// The retry strategy being applied (e.g. "expanded-context", "human-review").
        strategy: String,
        /// How many times the worst-case failure signature has been seen.
        repeated_count: u32,
    },

    // -- CI status events --
    /// CI polling started for a PR.
    CIPollingStarted {
        task_id: TaskId,
        repo: RepoName,
        pr_number: u64,
        pr_url: String,
    },

    /// CI check status update (from polling).
    CICheckUpdate {
        task_id: TaskId,
        repo: RepoName,
        pr_number: u64,
        /// Overall status: "pending", "pass", "fail".
        status: String,
        /// Summary of individual check results.
        summary: String,
    },

    /// All CI checks passed — PR will be merged.
    CIPassed {
        task_id: TaskId,
        repo: RepoName,
        pr_number: u64,
    },

    /// CI checks failed — dispatching ci_fixer agent.
    CIFailed {
        task_id: TaskId,
        repo: RepoName,
        pr_number: u64,
        /// Which attempt this is (1-based).
        attempt: u32,
        /// Max attempts allowed.
        max_attempts: u32,
        /// Summary of the CI failure.
        failure_summary: String,
    },

    /// CI fixer agent pushed a fix commit and is waiting for CI re-run.
    CIFixPushed {
        task_id: TaskId,
        repo: RepoName,
        pr_number: u64,
        attempt: u32,
    },

    /// CI retries exhausted — escalating to human review.
    CIEscalated {
        task_id: TaskId,
        repo: RepoName,
        pr_number: u64,
        attempts: u32,
        failure_summary: String,
    },

    // -- Remote sync events --
    /// Remote sync cycle started for a repository.
    SyncStarted { repo: RepoName, trigger: String },

    /// Local main branch updated to match remote.
    SyncMainUpdated {
        repo: RepoName,
        old_sha: String,
        new_sha: String,
    },

    /// An in-flight task branch was rebased onto updated main.
    SyncBranchRebased {
        repo: RepoName,
        branch: String,
        task_id: Option<TaskId>,
        success: bool,
        conflict_summary: Option<String>,
    },

    /// Remote sync cycle completed for a repository.
    SyncCompleted {
        repo: RepoName,
        branches_rebased: u32,
        conflicts: u32,
        duration_secs: f64,
    },
}

/// What kind of file system change was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
}

/// Which output stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Severity level for engine log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for PipelineEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ts = self.timestamp.format("%H:%M:%S%.3f");
        match &self.kind {
            EventKind::TaskStateChange {
                task_id,
                repo,
                from,
                to,
            } => write!(f, "[{ts}] {task_id} ({repo}): {from} -> {to}"),

            EventKind::AgentStarted {
                agent_id, task_id, ..
            } => write!(f, "[{ts}] {agent_id} started on {task_id}"),

            EventKind::AgentOutput {
                agent_id,
                stream,
                line,
                ..
            } => {
                let tag = match stream {
                    OutputStream::Stdout => "out",
                    OutputStream::Stderr => "err",
                };
                write!(f, "[{ts}] {agent_id} {tag}: {line}")
            }

            EventKind::AgentFinished {
                agent_id,
                success,
                elapsed_secs,
                ..
            } => {
                let status = if *success { "OK" } else { "FAIL" };
                write!(
                    f,
                    "[{ts}] {agent_id} finished ({status}, {elapsed_secs:.1}s)"
                )
            }

            EventKind::GateStarted { task_id, level } => {
                write!(f, "[{ts}] {task_id}: {level} started")
            }

            EventKind::GateOutput {
                check_name,
                stream,
                line,
                ..
            } => {
                let tag = match stream {
                    OutputStream::Stdout => "out",
                    OutputStream::Stderr => "err",
                };
                write!(f, "[{ts}] gate/{check_name} {tag}: {line}")
            }

            EventKind::GateCheckFinished {
                check_name, passed, ..
            } => {
                let status = if *passed { "PASS" } else { "FAIL" };
                write!(f, "[{ts}] gate/{check_name}: {status}")
            }

            EventKind::GateFinished {
                task_id,
                level,
                passed,
                duration_secs,
            } => {
                let status = if *passed { "PASS" } else { "FAIL" };
                write!(
                    f,
                    "[{ts}] {task_id}: {level} {status} ({duration_secs:.1}s)"
                )
            }

            EventKind::FileChanged {
                agent_id,
                path,
                kind,
                ..
            } => {
                let tag = match kind {
                    FileChangeKind::Created => "created",
                    FileChangeKind::Modified => "modified",
                    FileChangeKind::Deleted => "deleted",
                };
                write!(f, "[{ts}] {agent_id} file {tag}: {}", path.display())
            }

            EventKind::DiffUpdate {
                agent_id,
                files_changed,
                insertions,
                deletions,
                ..
            } => write!(
                f,
                "[{ts}] {agent_id} diff: {files_changed} files, +{insertions} -{deletions}"
            ),

            EventKind::EngineLog { level, message } => {
                let tag = match level {
                    LogLevel::Info => "INFO",
                    LogLevel::Warn => "WARN",
                    LogLevel::Error => "ERROR",
                };
                write!(f, "[{ts}] [{tag}] {message}")
            }

            EventKind::CheckpointSaved {
                task_id,
                repo,
                phase,
            } => write!(f, "[{ts}] {task_id} ({repo}): checkpoint saved at {phase}"),

            EventKind::SessionContinued {
                task_id,
                repo,
                session_id,
            } => write!(
                f,
                "[{ts}] {task_id} ({repo}): session continued ({session_id})"
            ),

            EventKind::FileConflictDetected {
                conflict, policy, ..
            } => {
                let policy_tag = match policy {
                    ConflictPolicy::WarnAndContinue => "warn",
                    ConflictPolicy::Serialize => "serialize",
                };
                write!(
                    f,
                    "[{ts}] CONFLICT ({policy_tag}): {} between {} and {} on {}",
                    conflict.path.display(),
                    conflict.first_agent,
                    conflict.second_agent,
                    conflict.repo,
                )
            }

            EventKind::CrossAgentNotification {
                source, message, ..
            } => write!(f, "[{ts}] {source} notifies: {message}"),

            EventKind::SharedMemoryWrite {
                agent_id,
                key,
                value,
            } => write!(f, "[{ts}] {agent_id} wrote shared[{key}] = {value}"),

            EventKind::TaskConvergenceDetected {
                task_id,
                strategy,
                repeated_count,
            } => write!(
                f,
                "[{ts}] {task_id}: convergence detected (strategy={strategy}, repeats={repeated_count})"
            ),

            EventKind::CIPollingStarted {
                task_id,
                repo,
                pr_number,
                ..
            } => write!(
                f,
                "[{ts}] {task_id} ({repo}): CI polling started for PR #{pr_number}"
            ),

            EventKind::CICheckUpdate {
                task_id,
                pr_number,
                status,
                summary,
                ..
            } => write!(
                f,
                "[{ts}] {task_id}: CI PR #{pr_number} status={status}: {summary}"
            ),

            EventKind::CIPassed {
                task_id, pr_number, ..
            } => write!(f, "[{ts}] {task_id}: CI PR #{pr_number} PASSED"),

            EventKind::CIFailed {
                task_id,
                pr_number,
                attempt,
                max_attempts,
                failure_summary,
                ..
            } => write!(
                f,
                "[{ts}] {task_id}: CI PR #{pr_number} FAILED (attempt {attempt}/{max_attempts}): {failure_summary}"
            ),

            EventKind::CIFixPushed {
                task_id,
                pr_number,
                attempt,
                ..
            } => write!(
                f,
                "[{ts}] {task_id}: CI fix pushed for PR #{pr_number} (attempt {attempt})"
            ),

            EventKind::CIEscalated {
                task_id,
                pr_number,
                attempts,
                failure_summary,
                ..
            } => write!(
                f,
                "[{ts}] {task_id}: CI ESCALATED for PR #{pr_number} after {attempts} attempts: {failure_summary}"
            ),

            EventKind::SyncStarted { repo, trigger } => {
                write!(f, "[{ts}] SYNC ({repo}): started (trigger={trigger})")
            }

            EventKind::SyncMainUpdated {
                repo,
                old_sha,
                new_sha,
            } => {
                let short_old = &old_sha[..7.min(old_sha.len())];
                let short_new = &new_sha[..7.min(new_sha.len())];
                write!(
                    f,
                    "[{ts}] SYNC ({repo}): main updated {short_old}..{short_new}"
                )
            }

            EventKind::SyncBranchRebased {
                repo,
                branch,
                success,
                ..
            } => {
                let status = if *success { "OK" } else { "CONFLICT" };
                write!(f, "[{ts}] SYNC ({repo}): rebase {branch} {status}")
            }

            EventKind::SyncCompleted {
                repo,
                branches_rebased,
                conflicts,
                duration_secs,
            } => write!(
                f,
                "[{ts}] SYNC ({repo}): completed ({branches_rebased} rebased, {conflicts} conflicts, {duration_secs:.1}s)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_display_task_state_change() {
        let event = PipelineEvent::new(EventKind::TaskStateChange {
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
            from: "pending".into(),
            to: "implementing".into(),
        });
        let s = event.to_string();
        assert!(s.contains("TASK-0001"));
        assert!(s.contains("pending -> implementing"));
    }

    #[test]
    fn event_serialize_roundtrip() {
        let event = PipelineEvent::new(EventKind::AgentFinished {
            agent_id: AgentId("agent-1-loom-TASK-0001".into()),
            task_id: TaskId(1),
            success: true,
            elapsed_secs: 42.5,
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PipelineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed.kind,
            EventKind::AgentFinished { success: true, .. }
        ));
    }

    #[test]
    fn gate_output_display() {
        let event = PipelineEvent::new(EventKind::GateOutput {
            task_id: TaskId(2),
            level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            stream: OutputStream::Stderr,
            line: "running 42 tests".into(),
        });
        let s = event.to_string();
        assert!(s.contains("gate/cargo_test err: running 42 tests"));
    }

    #[test]
    fn file_changed_display() {
        let event = PipelineEvent::new(EventKind::FileChanged {
            agent_id: AgentId("agent-1-loom-TASK-0001".into()),
            task_id: TaskId(1),
            path: std::path::PathBuf::from("src/main.rs"),
            kind: FileChangeKind::Modified,
        });
        let s = event.to_string();
        assert!(s.contains("file modified: src/main.rs"));
    }

    #[test]
    fn diff_update_display() {
        let event = PipelineEvent::new(EventKind::DiffUpdate {
            agent_id: AgentId("agent-1-loom-TASK-0001".into()),
            task_id: TaskId(1),
            files_changed: 3,
            insertions: 42,
            deletions: 7,
        });
        let s = event.to_string();
        assert!(s.contains("diff: 3 files, +42 -7"));
    }

    #[test]
    fn file_changed_serialize_roundtrip() {
        let event = PipelineEvent::new(EventKind::FileChanged {
            agent_id: AgentId("agent-1-loom-TASK-0001".into()),
            task_id: TaskId(1),
            path: std::path::PathBuf::from("src/lib.rs"),
            kind: FileChangeKind::Created,
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PipelineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed.kind,
            EventKind::FileChanged {
                kind: FileChangeKind::Created,
                ..
            }
        ));
    }

    #[test]
    fn engine_log_display() {
        let event = PipelineEvent::new(EventKind::EngineLog {
            level: LogLevel::Warn,
            message: "approaching budget limit".into(),
        });
        let s = event.to_string();
        assert!(s.contains("[WARN] approaching budget limit"));
    }

    #[test]
    fn file_conflict_detected_display() {
        use crate::coordination::{ConflictPolicy, FileConflict};
        let conflict = FileConflict {
            path: PathBuf::from("src/shared.rs"),
            first_agent: AgentId("agent-1".into()),
            second_agent: AgentId("agent-2".into()),
            first_task: TaskId(1),
            second_task: TaskId(2),
            repo: RepoName::new("loom"),
            detected_at: chrono::Utc::now(),
        };
        let event = PipelineEvent::new(EventKind::FileConflictDetected {
            conflict,
            policy: ConflictPolicy::WarnAndContinue,
        });
        let s = event.to_string();
        assert!(s.contains("CONFLICT (warn)"));
        assert!(s.contains("src/shared.rs"));
        assert!(s.contains("agent-1"));
        assert!(s.contains("agent-2"));
    }

    #[test]
    fn file_conflict_serialize_roundtrip() {
        use crate::coordination::{ConflictPolicy, FileConflict};
        let conflict = FileConflict {
            path: PathBuf::from("src/api.rs"),
            first_agent: AgentId("agent-1".into()),
            second_agent: AgentId("agent-2".into()),
            first_task: TaskId(1),
            second_task: TaskId(2),
            repo: RepoName::new("synth"),
            detected_at: chrono::Utc::now(),
        };
        let event = PipelineEvent::new(EventKind::FileConflictDetected {
            conflict,
            policy: ConflictPolicy::Serialize,
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PipelineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed.kind,
            EventKind::FileConflictDetected {
                policy: ConflictPolicy::Serialize,
                ..
            }
        ));
    }

    #[test]
    fn cross_agent_notification_display() {
        let event = PipelineEvent::new(EventKind::CrossAgentNotification {
            source: AgentId("agent-3".into()),
            source_task: TaskId(5),
            message: "API signature changed in foo.rs".into(),
            affected_files: vec![PathBuf::from("src/foo.rs")],
        });
        let s = event.to_string();
        assert!(s.contains("agent-3 notifies: API signature changed"));
    }

    #[test]
    fn shared_memory_write_display() {
        let event = PipelineEvent::new(EventKind::SharedMemoryWrite {
            agent_id: AgentId("agent-1".into()),
            key: "api_version".into(),
            value: "v2".into(),
        });
        let s = event.to_string();
        assert!(s.contains("shared[api_version] = v2"));
    }

    #[test]
    fn ci_polling_started_display() {
        let event = PipelineEvent::new(EventKind::CIPollingStarted {
            task_id: TaskId(23),
            repo: RepoName::new("loom"),
            pr_number: 42,
            pr_url: "https://github.com/org/loom/pull/42".into(),
        });
        let s = event.to_string();
        assert!(s.contains("TASK-0023"));
        assert!(s.contains("CI polling started"));
        assert!(s.contains("PR #42"));
    }

    #[test]
    fn ci_check_update_display() {
        let event = PipelineEvent::new(EventKind::CICheckUpdate {
            task_id: TaskId(23),
            repo: RepoName::new("loom"),
            pr_number: 42,
            status: "pending".into(),
            summary: "2 passed, 0 failed, 1 pending (total: 3)".into(),
        });
        let s = event.to_string();
        assert!(s.contains("TASK-0023"));
        assert!(s.contains("PR #42"));
        assert!(s.contains("status=pending"));
    }

    #[test]
    fn ci_passed_display() {
        let event = PipelineEvent::new(EventKind::CIPassed {
            task_id: TaskId(23),
            repo: RepoName::new("loom"),
            pr_number: 42,
        });
        let s = event.to_string();
        assert!(s.contains("TASK-0023"));
        assert!(s.contains("PR #42 PASSED"));
    }

    #[test]
    fn ci_failed_display() {
        let event = PipelineEvent::new(EventKind::CIFailed {
            task_id: TaskId(23),
            repo: RepoName::new("loom"),
            pr_number: 42,
            attempt: 2,
            max_attempts: 3,
            failure_summary: "test_neon failed".into(),
        });
        let s = event.to_string();
        assert!(s.contains("TASK-0023"));
        assert!(s.contains("PR #42 FAILED"));
        assert!(s.contains("attempt 2/3"));
        assert!(s.contains("test_neon failed"));
    }

    #[test]
    fn ci_fix_pushed_display() {
        let event = PipelineEvent::new(EventKind::CIFixPushed {
            task_id: TaskId(23),
            repo: RepoName::new("loom"),
            pr_number: 42,
            attempt: 1,
        });
        let s = event.to_string();
        assert!(s.contains("TASK-0023"));
        assert!(s.contains("CI fix pushed"));
        assert!(s.contains("PR #42"));
    }

    #[test]
    fn ci_escalated_display() {
        let event = PipelineEvent::new(EventKind::CIEscalated {
            task_id: TaskId(23),
            repo: RepoName::new("loom"),
            pr_number: 42,
            attempts: 3,
            failure_summary: "build failed".into(),
        });
        let s = event.to_string();
        assert!(s.contains("TASK-0023"));
        assert!(s.contains("CI ESCALATED"));
        assert!(s.contains("PR #42"));
        assert!(s.contains("3 attempts"));
    }

    #[test]
    fn sync_started_display() {
        let event = PipelineEvent::new(EventKind::SyncStarted {
            repo: RepoName::new("loom"),
            trigger: "eager".into(),
        });
        let s = event.to_string();
        assert!(s.contains("SYNC (loom)"));
        assert!(s.contains("trigger=eager"));
    }

    #[test]
    fn sync_main_updated_display() {
        let event = PipelineEvent::new(EventKind::SyncMainUpdated {
            repo: RepoName::new("loom"),
            old_sha: "abc1234def5678".into(),
            new_sha: "fed8765cba4321".into(),
        });
        let s = event.to_string();
        assert!(s.contains("SYNC (loom)"));
        assert!(s.contains("main updated"));
    }

    #[test]
    fn sync_branch_rebased_display() {
        let event = PipelineEvent::new(EventKind::SyncBranchRebased {
            repo: RepoName::new("loom"),
            branch: "auto/TASK-0001/loom/feature".into(),
            task_id: Some(TaskId(1)),
            success: true,
            conflict_summary: None,
        });
        let s = event.to_string();
        assert!(s.contains("SYNC (loom)"));
        assert!(s.contains("rebase"));
        assert!(s.contains("OK"));
    }

    #[test]
    fn sync_completed_display() {
        let event = PipelineEvent::new(EventKind::SyncCompleted {
            repo: RepoName::new("loom"),
            branches_rebased: 3,
            conflicts: 1,
            duration_secs: 2.5,
        });
        let s = event.to_string();
        assert!(s.contains("SYNC (loom)"));
        assert!(s.contains("3 rebased"));
        assert!(s.contains("1 conflicts"));
    }

    #[test]
    fn sync_event_serialize_roundtrip() {
        let event = PipelineEvent::new(EventKind::SyncStarted {
            repo: RepoName::new("synth"),
            trigger: "manual".into(),
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PipelineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed.kind, EventKind::SyncStarted { .. }));
    }

    #[test]
    fn ci_event_serialize_roundtrip() {
        let event = PipelineEvent::new(EventKind::CIPollingStarted {
            task_id: TaskId(10),
            repo: RepoName::new("synth"),
            pr_number: 99,
            pr_url: "https://github.com/org/synth/pull/99".into(),
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PipelineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed.kind,
            EventKind::CIPollingStarted { pr_number: 99, .. }
        ));
    }
}
