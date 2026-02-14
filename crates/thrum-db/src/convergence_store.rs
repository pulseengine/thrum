use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use thrum_core::convergence::FailureRecord;
use thrum_core::task::TaskId;

/// Convergence table: "{task_id}:{signature_hash}" -> JSON-serialized FailureRecord.
///
/// Keyed by task+signature so we can query all failure records for a task
/// and match against new failures efficiently.
pub const CONVERGENCE_TABLE: TableDefinition<&str, &str> = TableDefinition::new("convergence");

pub struct ConvergenceStore<'a> {
    db: &'a Database,
}

impl<'a> ConvergenceStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Store or update a failure record.
    ///
    /// If a record with the same key (task_id + signature hash) exists,
    /// it is replaced. The caller is responsible for calling
    /// `record_occurrence()` before storing.
    pub fn store(&self, record: &FailureRecord) -> Result<()> {
        let key = record_key(&record.task_id, &record.signature.error_hash);
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(CONVERGENCE_TABLE)?;
            let json = serde_json::to_string(record)?;
            table.insert(key.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Get all failure records for a task.
    pub fn get_for_task(&self, task_id: &TaskId) -> Result<Vec<FailureRecord>> {
        let prefix = format!("{}:", task_id);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CONVERGENCE_TABLE)?;
        let mut records = Vec::new();

        let iter = table.iter()?;
        for item in iter {
            let (key, value) = item?;
            if key.value().starts_with(&prefix) {
                let record: FailureRecord = serde_json::from_str(value.value())?;
                records.push(record);
            }
        }

        Ok(records)
    }

    /// Find a specific failure record by task ID and error hash.
    pub fn get(&self, task_id: &TaskId, error_hash: &str) -> Result<Option<FailureRecord>> {
        let key = record_key(task_id, error_hash);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(CONVERGENCE_TABLE)?;
        match table.get(key.as_str())? {
            Some(guard) => {
                let record: FailureRecord = serde_json::from_str(guard.value())?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Remove all convergence records for a task (e.g. after merge or reset).
    pub fn remove_for_task(&self, task_id: &TaskId) -> Result<u32> {
        let prefix = format!("{}:", task_id);
        let write_txn = self.db.begin_write()?;
        let count;
        {
            let mut table = write_txn.open_table(CONVERGENCE_TABLE)?;
            let mut to_remove = Vec::new();

            {
                let iter = table.iter()?;
                for item in iter {
                    let (key, _) = item?;
                    if key.value().starts_with(&prefix) {
                        to_remove.push(key.value().to_string());
                    }
                }
            }

            count = to_remove.len() as u32;
            for key in &to_remove {
                table.remove(key.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(count)
    }
}

fn record_key(task_id: &TaskId, error_hash: &str) -> String {
    format!("{task_id}:{error_hash}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::convergence::FailureSignature;
    use thrum_core::task::GateLevel;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        crate::open_db(&dir.path().join("test.redb")).unwrap()
    }

    #[test]
    fn store_and_get_for_task() {
        let db = test_db();
        let store = ConvergenceStore::new(&db);

        let sig = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            error_hash: "abc123def456".into(),
        };
        let record = FailureRecord::new(TaskId(1), sig, "test failed".into());
        store.store(&record).unwrap();

        let records = store.get_for_task(&TaskId(1)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].occurrence_count, 1);
        assert_eq!(records[0].latest_stderr, "test failed");
    }

    #[test]
    fn update_occurrence() {
        let db = test_db();
        let store = ConvergenceStore::new(&db);

        let sig = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_clippy".into(),
            error_hash: "clip123".into(),
        };
        let mut record = FailureRecord::new(TaskId(1), sig, "first error".into());
        store.store(&record).unwrap();

        record.record_occurrence("second error".into());
        store.store(&record).unwrap();

        let records = store.get_for_task(&TaskId(1)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].occurrence_count, 2);
        assert_eq!(records[0].latest_stderr, "second error");
    }

    #[test]
    fn multiple_signatures_per_task() {
        let db = test_db();
        let store = ConvergenceStore::new(&db);

        let sig1 = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            error_hash: "hash1".into(),
        };
        let sig2 = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_clippy".into(),
            error_hash: "hash2".into(),
        };

        store
            .store(&FailureRecord::new(TaskId(1), sig1, "err1".into()))
            .unwrap();
        store
            .store(&FailureRecord::new(TaskId(1), sig2, "err2".into()))
            .unwrap();

        let records = store.get_for_task(&TaskId(1)).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn get_specific_record() {
        let db = test_db();
        let store = ConvergenceStore::new(&db);

        let sig = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            error_hash: "specific_hash".into(),
        };
        store
            .store(&FailureRecord::new(TaskId(1), sig, "error".into()))
            .unwrap();

        let found = store.get(&TaskId(1), "specific_hash").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().signature.check_name, "cargo_test");

        let not_found = store.get(&TaskId(1), "other_hash").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn remove_for_task() {
        let db = test_db();
        let store = ConvergenceStore::new(&db);

        let sig = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            error_hash: "removable".into(),
        };
        store
            .store(&FailureRecord::new(TaskId(1), sig.clone(), "err".into()))
            .unwrap();
        store
            .store(&FailureRecord::new(TaskId(2), sig, "other".into()))
            .unwrap();

        let removed = store.remove_for_task(&TaskId(1)).unwrap();
        assert_eq!(removed, 1);

        let remaining = store.get_for_task(&TaskId(1)).unwrap();
        assert!(remaining.is_empty());

        // Task 2 should still have its record
        let task2 = store.get_for_task(&TaskId(2)).unwrap();
        assert_eq!(task2.len(), 1);
    }

    #[test]
    fn isolation_between_tasks() {
        let db = test_db();
        let store = ConvergenceStore::new(&db);

        let sig = FailureSignature {
            gate_level: GateLevel::Quality,
            check_name: "cargo_test".into(),
            error_hash: "same_hash".into(),
        };

        store
            .store(&FailureRecord::new(
                TaskId(1),
                sig.clone(),
                "task1 error".into(),
            ))
            .unwrap();
        store
            .store(&FailureRecord::new(TaskId(2), sig, "task2 error".into()))
            .unwrap();

        let task1_records = store.get_for_task(&TaskId(1)).unwrap();
        let task2_records = store.get_for_task(&TaskId(2)).unwrap();
        assert_eq!(task1_records.len(), 1);
        assert_eq!(task2_records.len(), 1);
        assert_eq!(task1_records[0].latest_stderr, "task1 error");
        assert_eq!(task2_records[0].latest_stderr, "task2 error");
    }
}
