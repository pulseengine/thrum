//! Agent-to-agent coordination hub for the parallel execution engine.
//!
//! The [`CoordinationHub`] listens for [`FileChanged`](thrum_core::event::EventKind::FileChanged)
//! events on the [`EventBus`] and maintains an in-memory map of which agents have
//! touched which files. When two agents in the same repo touch overlapping files,
//! a [`FileConflictDetected`](thrum_core::event::EventKind::FileConflictDetected)
//! event is emitted.
//!
//! Also manages:
//! - **Shared memory store**: agents can read/write key-value pairs visible to all.
//! - **Cross-agent notifications**: agents can publish messages for others to consume.
//!
//! The hub runs as a background tokio task, started alongside the parallel engine
//! and stopped via [`CancellationToken`].

use crate::event_bus::EventBus;
use std::collections::HashMap;
use std::sync::Arc;
use thrum_core::agent::AgentId;
use thrum_core::coordination::{
    ConflictPolicy, CrossAgentNotification, FileOwnershipMap, SharedMemoryEntry,
};
use thrum_core::event::EventKind;
use thrum_core::task::{RepoName, TaskId};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Shared coordination state accessible by all agents.
///
/// Protected by `RwLock` for concurrent read-heavy access. Writes are rare
/// (file touch on change events, shared memory writes).
pub struct CoordinationState {
    /// Tracks which agents have touched which files.
    pub file_ownership: FileOwnershipMap,
    /// Ephemeral shared memory visible to all concurrent agents.
    pub shared_memory: HashMap<String, SharedMemoryEntry>,
    /// Cross-agent notifications published during the session.
    pub notifications: Vec<CrossAgentNotification>,
    /// Detected file conflicts (for reporting).
    pub conflicts: Vec<thrum_core::coordination::FileConflict>,
    /// Policy for handling file conflicts.
    pub conflict_policy: ConflictPolicy,
}

impl CoordinationState {
    fn new(conflict_policy: ConflictPolicy) -> Self {
        Self {
            file_ownership: FileOwnershipMap::new(),
            shared_memory: HashMap::new(),
            notifications: Vec::new(),
            conflicts: Vec::new(),
            conflict_policy,
        }
    }
}

/// The coordination hub manages inter-agent coordination within a parallel
/// engine session.
///
/// Create one per `run_parallel()` invocation. It runs a background task that
/// listens for file change events and detects conflicts in real-time.
#[derive(Clone)]
pub struct CoordinationHub {
    state: Arc<RwLock<CoordinationState>>,
    event_bus: EventBus,
}

impl CoordinationHub {
    /// Create a new coordination hub with the given conflict policy.
    pub fn new(event_bus: EventBus, conflict_policy: ConflictPolicy) -> Self {
        Self {
            state: Arc::new(RwLock::new(CoordinationState::new(conflict_policy))),
            event_bus,
        }
    }

    /// Register an agent that is starting work.
    ///
    /// Call this when an agent is dispatched so the hub can track its files.
    pub async fn register_agent(&self, agent_id: AgentId, task_id: TaskId, repo: RepoName) {
        let mut state = self.state.write().await;
        state.file_ownership.register_agent(agent_id, task_id, repo);
    }

    /// Unregister an agent that has finished work.
    pub async fn unregister_agent(&self, agent_id: &AgentId) {
        let mut state = self.state.write().await;
        state.file_ownership.unregister_agent(agent_id);
    }

    /// Write a value to the shared memory store, visible to all agents.
    pub async fn write_shared_memory(&self, key: String, value: String, author: AgentId) {
        let entry = SharedMemoryEntry::new(key.clone(), value.clone(), author.clone());
        {
            let mut state = self.state.write().await;
            state.shared_memory.insert(key.clone(), entry);
        }
        self.event_bus.emit(EventKind::SharedMemoryWrite {
            agent_id: author,
            key,
            value,
        });
    }

    /// Read a value from the shared memory store.
    pub async fn read_shared_memory(&self, key: &str) -> Option<SharedMemoryEntry> {
        let state = self.state.read().await;
        state.shared_memory.get(key).cloned()
    }

    /// Publish a cross-agent notification.
    pub async fn publish_notification(&self, notification: CrossAgentNotification) {
        self.event_bus.emit(EventKind::CrossAgentNotification {
            source: notification.source.clone(),
            source_task: notification.source_task.clone(),
            message: notification.message.clone(),
            affected_files: notification.affected_files.clone(),
        });
        let mut state = self.state.write().await;
        state.notifications.push(notification);
    }

    /// Get all notifications published so far.
    pub async fn notifications(&self) -> Vec<CrossAgentNotification> {
        let state = self.state.read().await;
        state.notifications.clone()
    }

    /// Get all detected conflicts so far.
    pub async fn conflicts(&self) -> Vec<thrum_core::coordination::FileConflict> {
        let state = self.state.read().await;
        state.conflicts.clone()
    }

    /// Get a summary of the coordination state for logging.
    pub async fn summary(&self) -> CoordinationSummary {
        let state = self.state.read().await;
        CoordinationSummary {
            active_agents: state.file_ownership.agent_count(),
            shared_memory_entries: state.shared_memory.len(),
            notifications_count: state.notifications.len(),
            conflicts_count: state.conflicts.len(),
        }
    }

    /// Start the background conflict-detection listener.
    ///
    /// This task subscribes to the [`EventBus`] and watches for
    /// [`FileChanged`](EventKind::FileChanged) events. When an agent touches
    /// a file, it records the touch and checks for conflicts with other agents.
    ///
    /// Returns a [`JoinHandle`] that completes when `cancel` fires.
    pub fn start_conflict_listener(&self, cancel: CancellationToken) -> JoinHandle<()> {
        let state = Arc::clone(&self.state);
        let event_bus = self.event_bus.clone();
        let mut rx = self.event_bus.subscribe();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    result = rx.recv() => {
                        match result {
                            Ok(event) => {
                                if let EventKind::FileChanged {
                                    ref agent_id,
                                    ref task_id,
                                    ref path,
                                    ..
                                } = event.kind
                                {
                                    // Normalize path — strip any absolute prefix to get
                                    // a relative repo path for consistent comparison.
                                    let rel_path = path.clone();

                                    let mut s = state.write().await;
                                    let policy = s.conflict_policy;
                                    if let Some(conflict) =
                                        s.file_ownership.record_file_touch(agent_id, rel_path)
                                    {
                                        tracing::warn!(
                                            path = %conflict.path.display(),
                                            first = %conflict.first_agent,
                                            second = %conflict.second_agent,
                                            policy = ?policy,
                                            "file conflict detected"
                                        );

                                        event_bus.emit(EventKind::FileConflictDetected {
                                            conflict: conflict.clone(),
                                            policy,
                                        });

                                        s.conflicts.push(conflict);
                                    }

                                    // Drop lock before any potential await
                                    drop(s);

                                    let _ = task_id; // used in FileChanged match
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::debug!(
                                    skipped = n,
                                    "coordination listener lagged, some events missed"
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
            tracing::debug!("coordination conflict listener stopped");
        })
    }
}

/// Summary of coordination state for logging/status.
#[derive(Debug)]
pub struct CoordinationSummary {
    pub active_agents: usize,
    pub shared_memory_entries: usize,
    pub notifications_count: usize,
    pub conflicts_count: usize,
}

impl std::fmt::Display for CoordinationSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "agents={}, shared_mem={}, notifications={}, conflicts={}",
            self.active_agents,
            self.shared_memory_entries,
            self.notifications_count,
            self.conflicts_count,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::coordination::ConflictPolicy;
    use thrum_core::event::FileChangeKind;

    fn agent(name: &str) -> AgentId {
        AgentId(name.into())
    }

    fn task(id: i64) -> TaskId {
        TaskId(id)
    }

    fn repo(name: &str) -> RepoName {
        RepoName::new(name)
    }

    #[tokio::test]
    async fn register_and_unregister_agent() {
        let bus = EventBus::new();
        let hub = CoordinationHub::new(bus, ConflictPolicy::WarnAndContinue);

        hub.register_agent(agent("a1"), task(1), repo("loom")).await;
        assert_eq!(hub.summary().await.active_agents, 1);

        hub.unregister_agent(&agent("a1")).await;
        assert_eq!(hub.summary().await.active_agents, 0);
    }

    #[tokio::test]
    async fn shared_memory_write_and_read() {
        let bus = EventBus::new();
        let hub = CoordinationHub::new(bus, ConflictPolicy::WarnAndContinue);

        hub.write_shared_memory("api_version".into(), "v2".into(), agent("a1"))
            .await;

        let entry = hub.read_shared_memory("api_version").await;
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().value, "v2");

        let missing = hub.read_shared_memory("nonexistent").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn publish_notification() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let hub = CoordinationHub::new(bus, ConflictPolicy::WarnAndContinue);

        let notif = CrossAgentNotification::new(agent("a1"), task(1), "schema changed".into());
        hub.publish_notification(notif).await;

        let notifications = hub.notifications().await;
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].message, "schema changed");

        // Also emitted to the event bus
        let event = rx.recv().await.unwrap();
        assert!(matches!(
            event.kind,
            EventKind::CrossAgentNotification { .. }
        ));
    }

    #[tokio::test]
    async fn conflict_listener_detects_overlap() {
        let bus = EventBus::new();
        let hub = CoordinationHub::new(bus.clone(), ConflictPolicy::WarnAndContinue);
        let cancel = CancellationToken::new();

        // Register two agents in the same repo
        hub.register_agent(agent("a1"), task(1), repo("loom")).await;
        hub.register_agent(agent("a2"), task(2), repo("loom")).await;

        // Start the listener
        let handle = hub.start_conflict_listener(cancel.clone());

        // Subscribe to receive the conflict event
        let mut rx = bus.subscribe();

        // Agent 1 touches a file
        bus.emit(EventKind::FileChanged {
            agent_id: agent("a1"),
            task_id: task(1),
            path: "src/shared.rs".into(),
            kind: FileChangeKind::Modified,
        });

        // Small delay to let the listener process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Agent 2 touches the same file — should trigger conflict
        bus.emit(EventKind::FileChanged {
            agent_id: agent("a2"),
            task_id: task(2),
            path: "src/shared.rs".into(),
            kind: FileChangeKind::Modified,
        });

        // Wait for the conflict event
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut found_conflict = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(event)) => {
                    if matches!(event.kind, EventKind::FileConflictDetected { .. }) {
                        found_conflict = true;
                        break;
                    }
                }
                _ => continue,
            }
        }

        assert!(found_conflict, "expected FileConflictDetected event");

        // Also recorded in state
        let conflicts = hub.conflicts().await;
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, std::path::PathBuf::from("src/shared.rs"));

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn no_conflict_for_different_repos() {
        let bus = EventBus::new();
        let hub = CoordinationHub::new(bus.clone(), ConflictPolicy::WarnAndContinue);
        let cancel = CancellationToken::new();

        hub.register_agent(agent("a1"), task(1), repo("loom")).await;
        hub.register_agent(agent("a2"), task(2), repo("synth"))
            .await;

        let handle = hub.start_conflict_listener(cancel.clone());

        bus.emit(EventKind::FileChanged {
            agent_id: agent("a1"),
            task_id: task(1),
            path: "src/lib.rs".into(),
            kind: FileChangeKind::Modified,
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        bus.emit(EventKind::FileChanged {
            agent_id: agent("a2"),
            task_id: task(2),
            path: "src/lib.rs".into(),
            kind: FileChangeKind::Modified,
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let conflicts = hub.conflicts().await;
        assert!(conflicts.is_empty());

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn summary_reflects_state() {
        let bus = EventBus::new();
        let hub = CoordinationHub::new(bus, ConflictPolicy::Serialize);

        let summary = hub.summary().await;
        assert_eq!(summary.active_agents, 0);
        assert_eq!(summary.shared_memory_entries, 0);
        assert_eq!(summary.conflicts_count, 0);

        hub.register_agent(agent("a1"), task(1), repo("loom")).await;
        hub.write_shared_memory("k".into(), "v".into(), agent("a1"))
            .await;

        let summary = hub.summary().await;
        assert_eq!(summary.active_agents, 1);
        assert_eq!(summary.shared_memory_entries, 1);
    }
}
