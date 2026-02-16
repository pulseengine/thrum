use anyhow::{Context, Result};
use chrono::Utc;
use redb::{Database, ReadableTable, TableDefinition};
use thrum_core::task::{RepoName, Task, TaskId, TaskStatus};

/// Priority category for claiming the next task.
/// Checked in order: retryable failures first, then approved, then pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimCategory {
    /// Gate1/Gate2 failures that can be retried.
    RetryableFailed,
    /// Tasks approved by a human, ready for integration.
    Approved,
    /// New pending tasks.
    Pending,
}

/// Tasks table: i64 task ID -> JSON-serialized Task.
pub const TASKS_TABLE: TableDefinition<i64, &str> = TableDefinition::new("tasks");

/// Auto-increment counter table: "next_task_id" -> i64.
pub const COUNTER_TABLE: TableDefinition<&str, i64> = TableDefinition::new("counters");

const NEXT_ID_KEY: &str = "next_task_id";

pub struct TaskStore<'a> {
    db: &'a Database,
}

impl<'a> TaskStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Access the underlying database reference.
    pub fn db(&self) -> &'a Database {
        self.db
    }

    /// Insert a new task, assigning it an auto-incremented ID.
    pub fn insert(&self, mut task: Task) -> Result<Task> {
        let write_txn = self.db.begin_write()?;
        {
            let mut counter = write_txn.open_table(COUNTER_TABLE)?;
            let next_id = counter.get(NEXT_ID_KEY)?.map(|v| v.value()).unwrap_or(1);
            task.id = TaskId(next_id);
            counter.insert(NEXT_ID_KEY, next_id + 1)?;

            let json = serde_json::to_string(&task)?;
            let mut tasks = write_txn.open_table(TASKS_TABLE)?;
            tasks.insert(next_id, json.as_str())?;
        }
        write_txn.commit()?;
        Ok(task)
    }

    /// Update an existing task.
    pub fn update(&self, task: &Task) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut tasks = write_txn.open_table(TASKS_TABLE)?;
            // Verify it exists
            tasks
                .get(task.id.0)?
                .context(format!("task {} not found", task.id))?;
            let json = serde_json::to_string(task)?;
            tasks.insert(task.id.0, json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Get a task by ID.
    pub fn get(&self, id: &TaskId) -> Result<Option<Task>> {
        let read_txn = self.db.begin_read()?;
        let tasks = read_txn.open_table(TASKS_TABLE)?;
        match tasks.get(id.0)? {
            Some(guard) => {
                let task: Task = serde_json::from_str(guard.value())?;
                Ok(Some(task))
            }
            None => Ok(None),
        }
    }

    /// List all tasks, optionally filtered by status and/or repo.
    pub fn list(
        &self,
        status_filter: Option<&str>,
        repo_filter: Option<&RepoName>,
    ) -> Result<Vec<Task>> {
        let read_txn = self.db.begin_read()?;
        let tasks = read_txn.open_table(TASKS_TABLE)?;
        let mut result = Vec::new();

        let iter = tasks.iter()?;
        for entry in iter {
            let (_, value) = entry?;
            let task: Task = serde_json::from_str(value.value())?;

            if let Some(status) = status_filter
                && task.status.label() != status
            {
                continue;
            }
            if let Some(repo) = repo_filter
                && &task.repo != repo
            {
                continue;
            }
            result.push(task);
        }

        Ok(result)
    }

    /// Get the next pending task (FIFO by ID).
    pub fn next_pending(&self, repo_filter: Option<&RepoName>) -> Result<Option<Task>> {
        let tasks = self.list(Some("pending"), repo_filter)?;
        Ok(tasks.into_iter().next())
    }

    /// Get all tasks awaiting human approval.
    pub fn awaiting_approval(&self) -> Result<Vec<Task>> {
        self.list(Some("awaiting-approval"), None)
    }

    /// Get the next approved task ready for integration (FIFO by ID).
    pub fn next_approved(&self, repo_filter: Option<&RepoName>) -> Result<Option<Task>> {
        let tasks = self.list(Some("approved"), repo_filter)?;
        Ok(tasks.into_iter().next())
    }

    /// Get tasks in a failed gate state that can be retried.
    pub fn retryable_failures(&self, repo_filter: Option<&RepoName>) -> Result<Vec<Task>> {
        let mut result = Vec::new();
        for task in self.list(None, repo_filter)? {
            let is_failed = matches!(
                task.status,
                thrum_core::task::TaskStatus::Gate1Failed { .. }
                    | thrum_core::task::TaskStatus::Gate2Failed { .. }
            );
            if is_failed && task.can_retry() {
                result.push(task);
            }
        }
        Ok(result)
    }

    /// Atomically claim the next eligible task for a given category.
    ///
    /// Uses a single write transaction to scan, find, and update — redb's
    /// single-writer lock prevents double-dispatch across threads/processes.
    pub fn claim_next(
        &self,
        agent_id: &str,
        category: ClaimCategory,
        repo_filter: Option<&RepoName>,
    ) -> Result<Option<Task>> {
        let write_txn = self.db.begin_write()?;
        let result = {
            let mut tasks = write_txn.open_table(TASKS_TABLE)?;

            // Scan all tasks and find the first eligible one (FIFO by ID)
            let mut candidate: Option<Task> = None;
            {
                let iter = tasks.iter()?;
                for entry in iter {
                    let (_, value) = entry?;
                    let task: Task = serde_json::from_str(value.value())?;

                    // Apply repo filter
                    if let Some(repo) = repo_filter
                        && &task.repo != repo
                    {
                        continue;
                    }

                    let eligible = match category {
                        ClaimCategory::RetryableFailed => {
                            task.status.is_claimable_retry() && task.can_retry()
                        }
                        ClaimCategory::Approved => task.status.is_claimable_approved(),
                        ClaimCategory::Pending => task.status.is_claimable_pending(),
                    };

                    if eligible {
                        candidate = Some(task);
                        break;
                    }
                }
            }

            // If found, atomically update to Claimed
            if let Some(mut task) = candidate {
                task.status = TaskStatus::Claimed {
                    agent_id: agent_id.to_string(),
                    claimed_at: Utc::now(),
                };
                task.updated_at = Utc::now();
                let json = serde_json::to_string(&task)?;
                tasks.insert(task.id.0, json.as_str())?;
                Some(task)
            } else {
                None
            }
        };
        write_txn.commit()?;
        Ok(result)
    }

    /// Delete a task by ID. Returns true if the task existed.
    pub fn delete(&self, id: &TaskId) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed;
        {
            let mut tasks = write_txn.open_table(TASKS_TABLE)?;
            existed = tasks.remove(id.0)?.is_some();
        }
        write_txn.commit()?;
        Ok(existed)
    }

    /// Count tasks by status.
    pub fn status_counts(&self) -> Result<std::collections::HashMap<String, usize>> {
        let read_txn = self.db.begin_read()?;
        let tasks = read_txn.open_table(TASKS_TABLE)?;
        let mut counts = std::collections::HashMap::new();

        let iter = tasks.iter()?;
        for entry in iter {
            let (_, value) = entry?;
            let task: Task = serde_json::from_str(value.value())?;
            *counts.entry(task.status.label().to_string()).or_insert(0) += 1;
        }

        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::task::{CheckResult, GateLevel, GateReport, RepoName, TaskStatus};

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        crate::open_db(&dir.path().join("test.redb")).unwrap()
    }

    fn failing_gate(level: GateLevel) -> GateReport {
        GateReport {
            level,
            checks: vec![CheckResult {
                name: "test".into(),
                passed: false,
                stdout: String::new(),
                stderr: "fail".into(),
                exit_code: 1,
            }],
            passed: false,
            duration_secs: 0.5,
        }
    }

    #[test]
    fn insert_and_get() {
        let db = test_db();
        let store = TaskStore::new(&db);

        let task = Task::new(RepoName::new("loom"), "Test task".into(), "desc".into());
        let inserted = store.insert(task).unwrap();
        assert_eq!(inserted.id.0, 1);

        let fetched = store.get(&TaskId(1)).unwrap().unwrap();
        assert_eq!(fetched.title, "Test task");
    }

    #[test]
    fn auto_increment() {
        let db = test_db();
        let store = TaskStore::new(&db);

        let t1 = store
            .insert(Task::new(RepoName::new("loom"), "First".into(), "d".into()))
            .unwrap();
        let t2 = store
            .insert(Task::new(
                RepoName::new("meld"),
                "Second".into(),
                "d".into(),
            ))
            .unwrap();

        assert_eq!(t1.id.0, 1);
        assert_eq!(t2.id.0, 2);
    }

    #[test]
    fn list_with_filters() {
        let db = test_db();
        let store = TaskStore::new(&db);

        store
            .insert(Task::new(
                RepoName::new("loom"),
                "Loom task".into(),
                "d".into(),
            ))
            .unwrap();
        store
            .insert(Task::new(
                RepoName::new("meld"),
                "Meld task".into(),
                "d".into(),
            ))
            .unwrap();

        let all = store.list(None, None).unwrap();
        assert_eq!(all.len(), 2);

        let loom_only = store.list(None, Some(&RepoName::new("loom"))).unwrap();
        assert_eq!(loom_only.len(), 1);
        assert_eq!(loom_only[0].title, "Loom task");

        let pending = store.list(Some("pending"), None).unwrap();
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn update_status() {
        let db = test_db();
        let store = TaskStore::new(&db);

        let mut task = store
            .insert(Task::new(RepoName::new("synth"), "Task".into(), "d".into()))
            .unwrap();
        task.status = TaskStatus::Implementing {
            branch: "auto/TASK-0001/synth/task".into(),
            started_at: chrono::Utc::now(),
        };
        store.update(&task).unwrap();

        let fetched = store.get(&task.id).unwrap().unwrap();
        assert_eq!(fetched.status.label(), "implementing");
    }

    #[test]
    fn next_pending() {
        let db = test_db();
        let store = TaskStore::new(&db);

        store
            .insert(Task::new(RepoName::new("loom"), "First".into(), "d".into()))
            .unwrap();
        store
            .insert(Task::new(
                RepoName::new("loom"),
                "Second".into(),
                "d".into(),
            ))
            .unwrap();

        let next = store.next_pending(None).unwrap().unwrap();
        assert_eq!(next.title, "First");
    }

    #[test]
    fn claim_next_atomic() {
        let db = test_db();
        let store = TaskStore::new(&db);

        store
            .insert(Task::new(
                RepoName::new("loom"),
                "Task A".into(),
                "d".into(),
            ))
            .unwrap();
        store
            .insert(Task::new(
                RepoName::new("synth"),
                "Task B".into(),
                "d".into(),
            ))
            .unwrap();

        // First claim gets Task A
        let claimed = store
            .claim_next("agent-1", ClaimCategory::Pending, None)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.title, "Task A");
        assert_eq!(claimed.status.label(), "claimed");

        // Second claim skips the already-claimed Task A, gets Task B
        let claimed2 = store
            .claim_next("agent-2", ClaimCategory::Pending, None)
            .unwrap()
            .unwrap();
        assert_eq!(claimed2.title, "Task B");

        // Third claim: nothing left
        let none = store
            .claim_next("agent-3", ClaimCategory::Pending, None)
            .unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn claim_with_repo_filter() {
        let db = test_db();
        let store = TaskStore::new(&db);

        store
            .insert(Task::new(
                RepoName::new("loom"),
                "Loom task".into(),
                "d".into(),
            ))
            .unwrap();
        store
            .insert(Task::new(
                RepoName::new("synth"),
                "Synth task".into(),
                "d".into(),
            ))
            .unwrap();

        // Claim only from Synth
        let claimed = store
            .claim_next(
                "agent-1",
                ClaimCategory::Pending,
                Some(&RepoName::new("synth")),
            )
            .unwrap()
            .unwrap();
        assert_eq!(claimed.title, "Synth task");

        // Loom task still available
        let loom = store
            .claim_next(
                "agent-2",
                ClaimCategory::Pending,
                Some(&RepoName::new("loom")),
            )
            .unwrap()
            .unwrap();
        assert_eq!(loom.title, "Loom task");
    }

    #[test]
    fn claim_retryable() {
        let db = test_db();
        let store = TaskStore::new(&db);

        let mut task = store
            .insert(Task::new(
                RepoName::new("loom"),
                "Failing".into(),
                "d".into(),
            ))
            .unwrap();
        task.status = TaskStatus::Gate1Failed {
            report: failing_gate(GateLevel::Quality),
        };
        store.update(&task).unwrap();

        // Claim as retryable
        let claimed = store
            .claim_next("agent-1", ClaimCategory::RetryableFailed, None)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.title, "Failing");
        assert_eq!(claimed.status.label(), "claimed");
    }
}
