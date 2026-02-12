//! Pipeline event types for real-time observability.
//!
//! Events are emitted by the engine as tasks progress through the pipeline.
//! Consumers (TUI, SSE endpoint, JSONL logger) subscribe and render them.
//!
//! These are pure data types with no async runtime dependency — the
//! broadcast bus lives in `thrum-runner`.

use crate::agent::AgentId;
use crate::task::{GateLevel, RepoName, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

    /// Engine-level log message (info, warn, error).
    EngineLog { level: LogLevel, message: String },
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

            EventKind::EngineLog { level, message } => {
                let tag = match level {
                    LogLevel::Info => "INFO",
                    LogLevel::Warn => "WARN",
                    LogLevel::Error => "ERROR",
                };
                write!(f, "[{ts}] [{tag}] {message}")
            }
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
    fn engine_log_display() {
        let event = PipelineEvent::new(EventKind::EngineLog {
            level: LogLevel::Warn,
            message: "approaching budget limit".into(),
        });
        let s = event.to_string();
        assert!(s.contains("[WARN] approaching budget limit"));
    }
}
