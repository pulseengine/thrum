//! Persistent checkpoint store for agent session resumption.
//!
//! Stores one active checkpoint per task, keyed by `checkpoint:{task_id}`.
//! Checkpoints are saved after each gate pass and loaded on resume to
//! determine which pipeline phases can be skipped.

use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use thrum_core::checkpoint::{Checkpoint, CheckpointId};
use thrum_core::task::TaskId;

/// redb table: checkpoint ID string -> JSON-serialized Checkpoint.
pub const CHECKPOINT_TABLE: TableDefinition<&str, &str> = TableDefinition::new("checkpoints");

pub struct CheckpointStore<'a> {
    db: &'a Database,
}

impl<'a> CheckpointStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Save or update a checkpoint.
    pub fn save(&self, checkpoint: &Checkpoint) -> Result<()> {
        let json = serde_json::to_string(checkpoint)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(CHECKPOINT_TABLE)?;
            table.insert(checkpoint.id.0.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Load a checkpoint for a task.
    pub fn get(&self, task_id: &TaskId) -> Result<Option<Checkpoint>> {
        let id = CheckpointId::for_task(task_id);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CHECKPOINT_TABLE)?;
        match table.get(id.0.as_str())? {
            Some(guard) => {
                let checkpoint: Checkpoint = serde_json::from_str(guard.value())?;
                Ok(Some(checkpoint))
            }
            None => Ok(None),
        }
    }

    /// Remove a checkpoint (e.g., after task is merged or abandoned).
    pub fn remove(&self, task_id: &TaskId) -> Result<bool> {
        let id = CheckpointId::for_task(task_id);
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(CHECKPOINT_TABLE)?;
            table.remove(id.0.as_str())?.is_some()
        };
        write_txn.commit()?;
        Ok(removed)
    }

    /// List all active checkpoints.
    pub fn list(&self) -> Result<Vec<Checkpoint>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CHECKPOINT_TABLE)?;
        let mut checkpoints = Vec::new();

        for entry in table.iter()? {
            let (_, value) = entry?;
            let checkpoint: Checkpoint = serde_json::from_str(value.value())?;
            checkpoints.push(checkpoint);
        }

        Ok(checkpoints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::checkpoint::Checkpoint;
    use thrum_core::task::{CheckResult, GateLevel, GateReport, RepoName};

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        crate::open_db(&dir.path().join("test.redb")).unwrap()
    }

    fn sample_gate_report(level: GateLevel) -> GateReport {
        GateReport {
            level,
            checks: vec![CheckResult {
                name: "cargo_test".into(),
                passed: true,
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
            }],
            passed: true,
            duration_secs: 5.0,
        }
    }

    #[test]
    fn save_and_get_checkpoint() {
        let db = test_db();
        let store = CheckpointStore::new(&db);
        let task_id = TaskId(42);

        let cp = Checkpoint::after_implementation(
            task_id.clone(),
            RepoName::new("loom"),
            "auto/TASK-0042/loom/test".into(),
        );

        store.save(&cp).unwrap();
        let loaded = store.get(&task_id).unwrap().unwrap();
        assert_eq!(loaded.task_id, TaskId(42));
        assert_eq!(loaded.branch, "auto/TASK-0042/loom/test");
    }

    #[test]
    fn get_missing_returns_none() {
        let db = test_db();
        let store = CheckpointStore::new(&db);
        assert!(store.get(&TaskId(999)).unwrap().is_none());
    }

    #[test]
    fn save_updates_existing_checkpoint() {
        let db = test_db();
        let store = CheckpointStore::new(&db);
        let task_id = TaskId(1);

        let mut cp = Checkpoint::after_implementation(
            task_id.clone(),
            RepoName::new("synth"),
            "auto/TASK-0001/synth/feature".into(),
        );
        store.save(&cp).unwrap();

        // Advance and save again
        cp.advance_to_gate1(sample_gate_report(GateLevel::Quality));
        store.save(&cp).unwrap();

        let loaded = store.get(&task_id).unwrap().unwrap();
        assert!(loaded.gate1_passed());
    }

    #[test]
    fn remove_checkpoint() {
        let db = test_db();
        let store = CheckpointStore::new(&db);
        let task_id = TaskId(7);

        let cp = Checkpoint::after_implementation(
            task_id.clone(),
            RepoName::new("meld"),
            "auto/TASK-0007/meld/fix".into(),
        );
        store.save(&cp).unwrap();

        let removed = store.remove(&task_id).unwrap();
        assert!(removed);
        assert!(store.get(&task_id).unwrap().is_none());

        // Removing again returns false
        let removed_again = store.remove(&task_id).unwrap();
        assert!(!removed_again);
    }

    #[test]
    fn list_all_checkpoints() {
        let db = test_db();
        let store = CheckpointStore::new(&db);

        store
            .save(&Checkpoint::after_implementation(
                TaskId(1),
                RepoName::new("loom"),
                "branch-1".into(),
            ))
            .unwrap();
        store
            .save(&Checkpoint::after_implementation(
                TaskId(2),
                RepoName::new("synth"),
                "branch-2".into(),
            ))
            .unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
    }
}
