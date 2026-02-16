//! Agent-to-agent coordination types for parallel execution.
//!
//! Three coordination mechanisms:
//! 1. **Shared memory** — agents read/write [`SharedMemoryEntry`] items visible
//!    to all concurrent agents within the same engine session.
//! 2. **File conflict detection** — when two agents touch overlapping files,
//!    a [`FileConflict`] is produced for the engine to handle.
//! 3. **Cross-agent events** — agents can publish [`CrossAgentNotification`]
//!    messages that other agents receive via the EventBus.

use crate::agent::AgentId;
use crate::task::{RepoName, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Shared memory
// ---------------------------------------------------------------------------

/// A memory entry visible to all concurrent agents in a session.
///
/// Unlike task-scoped [`MemoryEntry`](crate::memory::MemoryEntry), these are
/// ephemeral — they live only for the duration of the engine run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedMemoryEntry {
    pub key: String,
    pub value: String,
    pub author: AgentId,
    pub created_at: DateTime<Utc>,
}

impl SharedMemoryEntry {
    pub fn new(key: String, value: String, author: AgentId) -> Self {
        Self {
            key,
            value,
            author,
            created_at: Utc::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// File conflict detection
// ---------------------------------------------------------------------------

/// Policy for resolving file conflicts between concurrent agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    /// Emit an event but let both agents continue (optimistic).
    WarnAndContinue,
    /// Serialize the later agent's work (acquire a lock, wait for first to finish).
    Serialize,
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        Self::WarnAndContinue
    }
}

/// A detected file conflict between two concurrent agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConflict {
    /// Path that both agents touched.
    pub path: PathBuf,
    /// The agent that touched the file first.
    pub first_agent: AgentId,
    /// The agent that touched the file second (triggered the conflict).
    pub second_agent: AgentId,
    /// Task IDs involved.
    pub first_task: TaskId,
    pub second_task: TaskId,
    /// Repository where the conflict occurred.
    pub repo: RepoName,
    /// When the conflict was detected.
    pub detected_at: DateTime<Utc>,
}

impl fmt::Display for FileConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "conflict on {} between {} ({}) and {} ({})",
            self.path.display(),
            self.first_agent,
            self.first_task,
            self.second_agent,
            self.second_task,
        )
    }
}

// ---------------------------------------------------------------------------
// Cross-agent notifications
// ---------------------------------------------------------------------------

/// A notification published by one agent for consumption by others.
///
/// Examples: "API signature changed in foo.rs", "new dependency added",
/// "schema migration required".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossAgentNotification {
    /// The agent that published this notification.
    pub source: AgentId,
    pub source_task: TaskId,
    /// Human-readable summary of what changed.
    pub message: String,
    /// Optional: the files this notification relates to.
    pub affected_files: Vec<PathBuf>,
    pub timestamp: DateTime<Utc>,
}

impl CrossAgentNotification {
    pub fn new(source: AgentId, source_task: TaskId, message: String) -> Self {
        Self {
            source,
            source_task,
            message,
            affected_files: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn with_files(mut self, files: Vec<PathBuf>) -> Self {
        self.affected_files = files;
        self
    }
}

// ---------------------------------------------------------------------------
// File ownership tracker (pure data, no async)
// ---------------------------------------------------------------------------

/// Per-agent record of which files it has touched, keyed by agent ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFileRecord {
    pub agent_id: AgentId,
    pub task_id: TaskId,
    pub repo: RepoName,
    pub files: HashSet<PathBuf>,
}

/// Tracks file ownership across all active agents.
///
/// This is a pure-data structure — thread safety is handled by the caller
/// (typically behind a `Mutex` or `RwLock` in the runner).
#[derive(Debug, Default)]
pub struct FileOwnershipMap {
    /// agent_id → set of files touched
    agents: HashMap<String, AgentFileRecord>,
}

impl FileOwnershipMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an agent in the ownership map.
    pub fn register_agent(&mut self, agent_id: AgentId, task_id: TaskId, repo: RepoName) {
        self.agents.insert(
            agent_id.0.clone(),
            AgentFileRecord {
                agent_id,
                task_id,
                repo,
                files: HashSet::new(),
            },
        );
    }

    /// Remove an agent from the ownership map (agent finished).
    pub fn unregister_agent(&mut self, agent_id: &AgentId) {
        self.agents.remove(&agent_id.0);
    }

    /// Record that an agent touched a file. Returns a conflict if another
    /// agent in the same repo already touched this file.
    pub fn record_file_touch(&mut self, agent_id: &AgentId, path: PathBuf) -> Option<FileConflict> {
        // Find the touching agent's record
        let touching_record = self.agents.get(&agent_id.0)?;
        let touching_repo = touching_record.repo.clone();
        let touching_task = touching_record.task_id.clone();

        // Check all other agents in the same repo for overlap
        let conflict = self
            .agents
            .values()
            .find(|record| {
                record.agent_id != *agent_id
                    && record.repo == touching_repo
                    && record.files.contains(&path)
            })
            .map(|first| FileConflict {
                path: path.clone(),
                first_agent: first.agent_id.clone(),
                second_agent: agent_id.clone(),
                first_task: first.task_id.clone(),
                second_task: touching_task.clone(),
                repo: touching_repo.clone(),
                detected_at: Utc::now(),
            });

        // Record the file touch regardless of conflict
        if let Some(record) = self.agents.get_mut(&agent_id.0) {
            record.files.insert(path);
        }

        conflict
    }

    /// Get all files touched by an agent.
    pub fn files_for_agent(&self, agent_id: &AgentId) -> Option<&HashSet<PathBuf>> {
        self.agents.get(&agent_id.0).map(|r| &r.files)
    }

    /// Get a snapshot of all active agent records.
    pub fn active_agents(&self) -> Vec<&AgentFileRecord> {
        self.agents.values().collect()
    }

    /// Number of actively tracked agents.
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentId {
        AgentId(name.into())
    }

    fn task(id: i64) -> TaskId {
        TaskId(id)
    }

    fn repo(name: &str) -> RepoName {
        RepoName::new(name)
    }

    #[test]
    fn shared_memory_entry_creation() {
        let entry = SharedMemoryEntry::new("api_version".into(), "v2".into(), agent("agent-1"));
        assert_eq!(entry.key, "api_version");
        assert_eq!(entry.value, "v2");
    }

    #[test]
    fn file_conflict_display() {
        let conflict = FileConflict {
            path: PathBuf::from("src/lib.rs"),
            first_agent: agent("agent-1"),
            second_agent: agent("agent-2"),
            first_task: task(1),
            second_task: task(2),
            repo: repo("loom"),
            detected_at: Utc::now(),
        };
        let s = conflict.to_string();
        assert!(s.contains("src/lib.rs"));
        assert!(s.contains("agent-1"));
        assert!(s.contains("agent-2"));
    }

    #[test]
    fn cross_agent_notification_with_files() {
        let notif =
            CrossAgentNotification::new(agent("agent-1"), task(1), "API signature changed".into())
                .with_files(vec![PathBuf::from("src/api.rs")]);
        assert_eq!(notif.affected_files.len(), 1);
        assert_eq!(notif.message, "API signature changed");
    }

    #[test]
    fn ownership_no_conflict_different_files() {
        let mut map = FileOwnershipMap::new();
        map.register_agent(agent("a1"), task(1), repo("loom"));
        map.register_agent(agent("a2"), task(2), repo("loom"));

        let c1 = map.record_file_touch(&agent("a1"), PathBuf::from("src/foo.rs"));
        assert!(c1.is_none());

        let c2 = map.record_file_touch(&agent("a2"), PathBuf::from("src/bar.rs"));
        assert!(c2.is_none());
    }

    #[test]
    fn ownership_detects_conflict_same_file() {
        let mut map = FileOwnershipMap::new();
        map.register_agent(agent("a1"), task(1), repo("loom"));
        map.register_agent(agent("a2"), task(2), repo("loom"));

        let c1 = map.record_file_touch(&agent("a1"), PathBuf::from("src/lib.rs"));
        assert!(c1.is_none());

        let c2 = map.record_file_touch(&agent("a2"), PathBuf::from("src/lib.rs"));
        assert!(c2.is_some());

        let conflict = c2.unwrap();
        assert_eq!(conflict.first_agent, agent("a1"));
        assert_eq!(conflict.second_agent, agent("a2"));
        assert_eq!(conflict.path, PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn ownership_no_conflict_different_repos() {
        let mut map = FileOwnershipMap::new();
        map.register_agent(agent("a1"), task(1), repo("loom"));
        map.register_agent(agent("a2"), task(2), repo("synth"));

        map.record_file_touch(&agent("a1"), PathBuf::from("src/lib.rs"));
        let c2 = map.record_file_touch(&agent("a2"), PathBuf::from("src/lib.rs"));
        assert!(c2.is_none());
    }

    #[test]
    fn ownership_unregister_clears_agent() {
        let mut map = FileOwnershipMap::new();
        map.register_agent(agent("a1"), task(1), repo("loom"));
        map.record_file_touch(&agent("a1"), PathBuf::from("src/lib.rs"));

        map.unregister_agent(&agent("a1"));
        assert_eq!(map.agent_count(), 0);

        // A new agent touching the same file should not conflict
        map.register_agent(agent("a2"), task(2), repo("loom"));
        let c = map.record_file_touch(&agent("a2"), PathBuf::from("src/lib.rs"));
        assert!(c.is_none());
    }

    #[test]
    fn ownership_files_for_agent() {
        let mut map = FileOwnershipMap::new();
        map.register_agent(agent("a1"), task(1), repo("loom"));
        map.record_file_touch(&agent("a1"), PathBuf::from("src/foo.rs"));
        map.record_file_touch(&agent("a1"), PathBuf::from("src/bar.rs"));

        let files = map.files_for_agent(&agent("a1")).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&PathBuf::from("src/foo.rs")));
        assert!(files.contains(&PathBuf::from("src/bar.rs")));
    }

    #[test]
    fn conflict_policy_default_is_warn() {
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::WarnAndContinue);
    }

    #[test]
    fn ownership_same_agent_no_self_conflict() {
        let mut map = FileOwnershipMap::new();
        map.register_agent(agent("a1"), task(1), repo("loom"));

        map.record_file_touch(&agent("a1"), PathBuf::from("src/lib.rs"));
        // Same agent touching same file again — no conflict
        let c = map.record_file_touch(&agent("a1"), PathBuf::from("src/lib.rs"));
        assert!(c.is_none());
    }
}
