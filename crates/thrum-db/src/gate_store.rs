use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use thrum_core::task::{GateReport, TaskId};

/// Gate results table: "{task_id}:{gate_level}" -> JSON-serialized GateReport.
pub const GATES_TABLE: TableDefinition<&str, &str> = TableDefinition::new("gate_results");

pub struct GateStore<'a> {
    db: &'a Database,
}

impl<'a> GateStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    fn key(task_id: &TaskId, gate_label: &str) -> String {
        format!("{}:{}", task_id.0, gate_label)
    }

    /// Store a gate report for a task.
    pub fn store(&self, task_id: &TaskId, report: &GateReport) -> Result<()> {
        let key = Self::key(task_id, &format!("{}", report.level));
        let json = serde_json::to_string(report)?;

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(GATES_TABLE)?;
            table.insert(key.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Get a specific gate report for a task.
    pub fn get(&self, task_id: &TaskId, gate_label: &str) -> Result<Option<GateReport>> {
        let key = Self::key(task_id, gate_label);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(GATES_TABLE)?;

        match table.get(key.as_str())? {
            Some(guard) => {
                let report: GateReport = serde_json::from_str(guard.value())?;
                Ok(Some(report))
            }
            None => Ok(None),
        }
    }

    /// Get all gate reports for a task.
    pub fn get_all_for_task(&self, task_id: &TaskId) -> Result<Vec<GateReport>> {
        let prefix = format!("{}:", task_id.0);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(GATES_TABLE)?;
        let mut reports = Vec::new();

        let iter = table.iter()?;
        for entry in iter {
            let (key, value) = entry?;
            if key.value().starts_with(&prefix) {
                let report: GateReport = serde_json::from_str(value.value())?;
                reports.push(report);
            }
        }

        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::task::{CheckResult, GateLevel};

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        crate::open_db(&dir.path().join("test.redb")).unwrap()
    }

    #[test]
    fn store_and_retrieve() {
        let db = test_db();
        let store = GateStore::new(&db);
        let task_id = TaskId(1);

        let report = GateReport {
            level: GateLevel::Quality,
            checks: vec![CheckResult {
                name: "cargo_test".into(),
                passed: true,
                stdout: "ok".into(),
                stderr: String::new(),
                exit_code: 0,
            }],
            passed: true,
            duration_secs: 12.5,
        };

        store.store(&task_id, &report).unwrap();
        let fetched = store.get(&task_id, "Gate 1: Quality").unwrap().unwrap();
        assert!(fetched.passed);
        assert_eq!(fetched.checks.len(), 1);
    }
}
