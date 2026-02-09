use crate::task::{RepoName, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static AGENT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Unique identifier for a parallel agent instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl AgentId {
    /// Generate a new agent ID: `agent-{counter}-{repo}-{task_id}`.
    pub fn generate(repo: &RepoName, task_id: &TaskId) -> Self {
        let n = AGENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!("agent-{n}-{repo}-{task_id}"))
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Tracks the lifecycle of a single agent invocation within the parallel engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent_id: AgentId,
    pub task_id: TaskId,
    pub repo: RepoName,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Working directory used by this agent (main repo dir or worktree).
    pub work_dir: PathBuf,
}

impl AgentSession {
    pub fn new(agent_id: AgentId, task_id: TaskId, repo: RepoName, work_dir: PathBuf) -> Self {
        Self {
            agent_id,
            task_id,
            repo,
            started_at: Utc::now(),
            finished_at: None,
            work_dir,
        }
    }

    pub fn finish(&mut self) {
        self.finished_at = Some(Utc::now());
    }

    pub fn elapsed_secs(&self) -> f64 {
        let end = self.finished_at.unwrap_or_else(Utc::now);
        (end - self.started_at).num_milliseconds() as f64 / 1000.0
    }
}
