//! A2A (Agent-to-Agent) protocol types for Thrum.
//!
//! Implements the A2A protocol specification for inter-agent communication.
//! These are pure data types with serde serialization — no async runtime
//! dependency. HTTP handlers live in `thrum-api`.
//!
//! The A2A protocol complements MCP (agent-to-tool) with agent-to-agent
//! coordination: agent discovery via Agent Cards, task submission via
//! JSON-RPC 2.0, and real-time streaming via SSE.

use crate::task::{Task, TaskId, TaskStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

// ─── JSON-RPC 2.0 Envelope ──────────────────────────────────────────────

/// JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// A2A-specific: task not found.
pub const TASK_NOT_FOUND: i64 = -32001;
/// A2A-specific: task is in a terminal state and cannot be canceled.
pub const TASK_NOT_CANCELABLE: i64 = -32002;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: serde_json::Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ─── A2A Task Model ─────────────────────────────────────────────────────

/// A2A task state, mapped from Thrum's 12-variant `TaskStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum A2aTaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    InputRequired,
    Rejected,
}

impl A2aTaskState {
    /// Map a Thrum `TaskStatus` to the A2A state model.
    pub fn from_thrum_status(status: &TaskStatus) -> Self {
        match status {
            TaskStatus::Pending => A2aTaskState::Submitted,
            TaskStatus::Claimed { .. } => A2aTaskState::Submitted,
            TaskStatus::Implementing { .. } => A2aTaskState::Working,
            TaskStatus::Gate1Failed { .. } => A2aTaskState::Failed,
            TaskStatus::Reviewing { .. } => A2aTaskState::Working,
            TaskStatus::Gate2Failed { .. } => A2aTaskState::Failed,
            TaskStatus::AwaitingApproval { .. } => A2aTaskState::InputRequired,
            TaskStatus::Approved => A2aTaskState::Working,
            TaskStatus::Integrating => A2aTaskState::Working,
            TaskStatus::Gate3Failed { .. } => A2aTaskState::Failed,
            TaskStatus::AwaitingCI { .. } => A2aTaskState::Working,
            TaskStatus::CIFailed { .. } => A2aTaskState::Failed,
            TaskStatus::Merged { .. } => A2aTaskState::Completed,
            TaskStatus::Rejected { .. } => A2aTaskState::Rejected,
        }
    }
}

/// A2A message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum A2aRole {
    User,
    Agent,
}

/// A2A message part (tagged union).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum A2aPart {
    Text {
        text: String,
    },
    Data {
        data: serde_json::Value,
    },
    File {
        url: Option<String>,
        raw: Option<String>,
        mime_type: Option<String>,
    },
}

/// A2A message exchanged between user and agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aMessage {
    pub message_id: String,
    pub role: A2aRole,
    pub parts: Vec<A2aPart>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// A2A artifact produced by an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aArtifact {
    pub artifact_id: String,
    pub name: String,
    pub parts: Vec<A2aPart>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// A2A task status with timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aTaskStatus {
    pub state: A2aTaskState,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Full A2A task representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aTask {
    pub id: String,
    pub context_id: String,
    pub status: A2aTaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<A2aArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<A2aMessage>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl A2aTask {
    /// Convert a Thrum `Task` into an A2A task.
    pub fn from_thrum_task(task: &Task) -> Self {
        let state = A2aTaskState::from_thrum_status(&task.status);
        let message = match &task.status {
            TaskStatus::Implementing { branch, .. } => Some(format!("Working on branch {branch}")),
            TaskStatus::Gate1Failed { report }
            | TaskStatus::Gate2Failed { report }
            | TaskStatus::Gate3Failed { report } => {
                let failed_checks: Vec<&str> = report
                    .checks
                    .iter()
                    .filter(|c| !c.passed)
                    .map(|c| c.name.as_str())
                    .collect();
                Some(format!("Gate failed: {}", failed_checks.join(", ")))
            }
            TaskStatus::Reviewing { reviewer_output } => {
                Some(format!("Under review: {}", truncate(reviewer_output, 100)))
            }
            TaskStatus::AwaitingApproval { .. } => Some("Awaiting human approval".into()),
            TaskStatus::Merged { commit_sha } => Some(format!("Merged as {commit_sha}")),
            TaskStatus::Rejected { feedback } => {
                Some(format!("Rejected: {}", truncate(feedback, 100)))
            }
            _ => None,
        };

        let mut metadata = HashMap::new();
        metadata.insert(
            "repo".into(),
            serde_json::Value::String(task.repo.to_string()),
        );
        metadata.insert(
            "thrum_status".into(),
            serde_json::Value::String(task.status.label().to_string()),
        );
        if let Some(ref req_id) = task.requirement_id {
            metadata.insert(
                "requirement_id".into(),
                serde_json::Value::String(req_id.clone()),
            );
        }
        if task.retry_count > 0 {
            metadata.insert("retry_count".into(), serde_json::json!(task.retry_count));
        }

        // Build history from the task description as the initial user message
        let initial_message = A2aMessage {
            message_id: format!("msg-init-{}", task.id.0),
            role: A2aRole::User,
            parts: vec![A2aPart::Text {
                text: format!("{}\n\n{}", task.title, task.description),
            }],
            metadata: HashMap::new(),
        };

        Self {
            id: a2a_task_id(&task.id),
            context_id: a2a_context_id(task),
            status: A2aTaskStatus {
                state,
                timestamp: task.updated_at,
                message,
            },
            artifacts: Vec::new(),
            history: vec![initial_message],
            metadata,
        }
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

// ─── Agent Card ─────────────────────────────────────────────────────────

/// A2A Agent Card — describes agent capabilities for discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    #[serde(rename = "supportedInterfaces")]
    pub supported_interfaces: Vec<String>,
    pub capabilities: AgentCapabilities,
    #[serde(rename = "defaultInputModes")]
    pub default_input_modes: Vec<String>,
    #[serde(rename = "defaultOutputModes")]
    pub default_output_modes: Vec<String>,
    pub skills: Vec<AgentSkill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "inputModes")]
    pub input_modes: Vec<String>,
    #[serde(rename = "outputModes")]
    pub output_modes: Vec<String>,
}

impl AgentCard {
    /// Build the default Thrum agent card with 3 skills.
    pub fn thrum_default(base_url: &str) -> Self {
        Self {
            name: "Thrum".into(),
            description: "Autonomous AI-driven development orchestrator. Manages tasks through a gated pipeline with quality, proof, and integration checks.".into(),
            url: format!("{base_url}/a2a"),
            version: env!("CARGO_PKG_VERSION").into(),
            supported_interfaces: vec!["a2a".into()],
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
            },
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into(), "application/json".into()],
            skills: vec![
                AgentSkill {
                    id: "implement".into(),
                    name: "Implement".into(),
                    description: "Submit a development task for autonomous implementation through the quality-gated pipeline.".into(),
                    input_modes: vec!["text/plain".into()],
                    output_modes: vec!["text/plain".into(), "application/json".into()],
                },
                AgentSkill {
                    id: "review".into(),
                    name: "Review".into(),
                    description: "Check the status and review output of a task in the pipeline.".into(),
                    input_modes: vec!["text/plain".into()],
                    output_modes: vec!["application/json".into()],
                },
                AgentSkill {
                    id: "status".into(),
                    name: "Status".into(),
                    description: "List tasks and their current pipeline state.".into(),
                    input_modes: vec!["text/plain".into()],
                    output_modes: vec!["application/json".into()],
                },
            ],
            provider: Some("Thrum Orchestrator".into()),
        }
    }
}

// ─── Method Params ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageParams {
    pub message: A2aMessage,
    #[serde(default)]
    pub context_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetTaskParams {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTasksParams {
    #[serde(default)]
    pub context_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelTaskParams {
    pub task_id: String,
}

// ─── SSE Stream Events ──────────────────────────────────────────────────

/// A2A SSE stream event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum A2aStreamEvent {
    Task {
        task: A2aTask,
    },
    Message {
        message: A2aMessage,
    },
    StatusUpdate {
        task_id: String,
        status: A2aTaskStatus,
    },
    ArtifactUpdate {
        task_id: String,
        artifact: A2aArtifact,
    },
}

// ─── ID Helpers ─────────────────────────────────────────────────────────

static MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(1);
static ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Convert a Thrum `TaskId` to an A2A task ID string.
pub fn a2a_task_id(id: &TaskId) -> String {
    format!("thrum-{}", id.0)
}

/// Parse a Thrum `TaskId` from an A2A task ID string.
pub fn parse_thrum_task_id(a2a_id: &str) -> Option<TaskId> {
    a2a_id
        .strip_prefix("thrum-")?
        .parse::<i64>()
        .ok()
        .map(TaskId)
}

/// Derive an A2A context ID from a Thrum task.
pub fn a2a_context_id(task: &Task) -> String {
    task.context_id
        .clone()
        .unwrap_or_else(|| format!("repo-{}", task.repo))
}

/// Generate a unique message ID.
pub fn next_message_id() -> String {
    format!("msg-{}", MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Generate a unique artifact ID.
pub fn next_artifact_id() -> String {
    format!(
        "artifact-{}",
        ARTIFACT_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{CheckResult, GateLevel, GateReport, RepoName};

    #[test]
    fn state_mapping_exhaustive() {
        // Verify every TaskStatus variant maps to a valid A2A state
        let statuses: Vec<TaskStatus> = vec![
            TaskStatus::Pending,
            TaskStatus::Claimed {
                agent_id: "a".into(),
                claimed_at: Utc::now(),
            },
            TaskStatus::Implementing {
                branch: "b".into(),
                started_at: Utc::now(),
            },
            TaskStatus::Gate1Failed {
                report: test_report(),
            },
            TaskStatus::Reviewing {
                reviewer_output: "ok".into(),
            },
            TaskStatus::Gate2Failed {
                report: test_report(),
            },
            TaskStatus::AwaitingApproval {
                summary: crate::task::CheckpointSummary {
                    diff_summary: String::new(),
                    reviewer_output: String::new(),
                    gate1_report: test_report(),
                    gate2_report: None,
                },
            },
            TaskStatus::Approved,
            TaskStatus::Integrating,
            TaskStatus::Gate3Failed {
                report: test_report(),
            },
            TaskStatus::Merged {
                commit_sha: "abc".into(),
            },
            TaskStatus::Rejected {
                feedback: "nope".into(),
            },
        ];

        let expected = [
            A2aTaskState::Submitted,     // Pending
            A2aTaskState::Submitted,     // Claimed
            A2aTaskState::Working,       // Implementing
            A2aTaskState::Failed,        // Gate1Failed
            A2aTaskState::Working,       // Reviewing
            A2aTaskState::Failed,        // Gate2Failed
            A2aTaskState::InputRequired, // AwaitingApproval
            A2aTaskState::Working,       // Approved
            A2aTaskState::Working,       // Integrating
            A2aTaskState::Failed,        // Gate3Failed
            A2aTaskState::Completed,     // Merged
            A2aTaskState::Rejected,      // Rejected
        ];

        for (status, expected_state) in statuses.iter().zip(expected.iter()) {
            let actual = A2aTaskState::from_thrum_status(status);
            assert_eq!(
                actual,
                *expected_state,
                "status {:?} mapped to {:?}, expected {:?}",
                status.label(),
                actual,
                expected_state,
            );
        }
    }

    #[test]
    fn id_roundtrip() {
        let id = TaskId(42);
        let a2a = a2a_task_id(&id);
        assert_eq!(a2a, "thrum-42");
        let parsed = parse_thrum_task_id(&a2a).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn id_parse_invalid() {
        assert!(parse_thrum_task_id("invalid-42").is_none());
        assert!(parse_thrum_task_id("thrum-abc").is_none());
        assert!(parse_thrum_task_id("").is_none());
    }

    #[test]
    fn context_id_from_task() {
        let mut task = Task::new(RepoName::new("loom"), "Test".into(), "desc".into());
        // No context_id set — falls back to repo name
        assert_eq!(a2a_context_id(&task), "repo-loom");

        // With explicit context_id
        task.context_id = Some("sprint-42".into());
        assert_eq!(a2a_context_id(&task), "sprint-42");
    }

    #[test]
    fn agent_card_valid() {
        let card = AgentCard::thrum_default("http://localhost:3000");
        assert_eq!(card.name, "Thrum");
        assert_eq!(card.url, "http://localhost:3000/a2a");
        assert_eq!(card.skills.len(), 3);
        assert!(card.capabilities.streaming);
        assert!(!card.capabilities.push_notifications);

        // Verify skills
        let skill_ids: Vec<&str> = card.skills.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(skill_ids, vec!["implement", "review", "status"]);
    }

    #[test]
    fn agent_card_serialization() {
        let card = AgentCard::thrum_default("http://localhost:3000");
        let json = serde_json::to_string(&card).unwrap();
        let parsed: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "Thrum");
        assert_eq!(parsed.skills.len(), 3);
    }

    #[test]
    fn jsonrpc_response_success() {
        let resp = JsonRpcResponse::success(serde_json::json!(1), serde_json::json!({"ok": true}));
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());

        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn jsonrpc_response_error() {
        let resp = JsonRpcResponse::error(serde_json::json!(1), METHOD_NOT_FOUND, "not found");
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
        assert_eq!(resp.error.as_ref().unwrap().code, METHOD_NOT_FOUND);

        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("\"result\""));
    }

    #[test]
    fn a2a_task_from_thrum() {
        let mut task = Task::new(
            RepoName::new("loom"),
            "Add feature X".into(),
            "Details here".into(),
        );
        task.id = TaskId(7);
        let a2a = A2aTask::from_thrum_task(&task);
        assert_eq!(a2a.id, "thrum-7");
        assert_eq!(a2a.context_id, "repo-loom");
        assert_eq!(a2a.status.state, A2aTaskState::Submitted);
        assert_eq!(a2a.history.len(), 1);
        assert_eq!(a2a.history[0].role, A2aRole::User);
        assert_eq!(a2a.metadata["repo"], "loom");
    }

    #[test]
    fn message_ids_unique() {
        let id1 = next_message_id();
        let id2 = next_message_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("msg-"));
    }

    #[test]
    fn artifact_ids_unique() {
        let id1 = next_artifact_id();
        let id2 = next_artifact_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("artifact-"));
    }

    #[test]
    fn stream_event_serialization() {
        let event = A2aStreamEvent::StatusUpdate {
            task_id: "thrum-1".into(),
            status: A2aTaskStatus {
                state: A2aTaskState::Working,
                timestamp: Utc::now(),
                message: Some("implementing".into()),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"statusupdate\""));
        let parsed: A2aStreamEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, A2aStreamEvent::StatusUpdate { .. }));
    }

    #[test]
    fn send_message_params_deserialize() {
        let json = r#"{
            "message": {
                "message_id": "m1",
                "role": "user",
                "parts": [{"type": "text", "text": "implement X"}]
            },
            "context_id": "ctx-1"
        }"#;
        let params: SendMessageParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.context_id, Some("ctx-1".into()));
        assert_eq!(params.message.parts.len(), 1);
    }

    fn test_report() -> GateReport {
        GateReport {
            level: GateLevel::Quality,
            checks: vec![CheckResult {
                name: "test".into(),
                passed: false,
                stdout: String::new(),
                stderr: "fail".into(),
                exit_code: 1,
            }],
            passed: false,
            duration_secs: 1.0,
        }
    }
}
