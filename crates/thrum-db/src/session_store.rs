//! Persistent session ID store for agent session continuation.
//!
//! Maps `session:{task_id}` to the last known session ID from the agent backend.
//! When a task is retried after timeout or failure, the stored session ID is
//! used to resume the agent's session (preserving conversation context).
//!
//! Claude Code uses `--resume {session_id}`, OpenCode uses `-s {session_id}`.

use anyhow::Result;
use redb::{Database, TableDefinition};
use thrum_core::task::TaskId;

/// redb table: `session:{task_id}` → session ID string.
pub const SESSION_TABLE: TableDefinition<&str, &str> = TableDefinition::new("sessions");

pub struct SessionStore<'a> {
    db: &'a Database,
}

impl<'a> SessionStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Store the session ID for a task. Overwrites any existing value.
    pub fn save(&self, task_id: &TaskId, session_id: &str) -> Result<()> {
        let key = format!("session:{}", task_id.0);
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(SESSION_TABLE)?;
            table.insert(key.as_str(), session_id)?;
        }
        write_txn.commit()?;
        tracing::debug!(task_id = %task_id, session_id, "stored session ID");
        Ok(())
    }

    /// Retrieve the stored session ID for a task, if any.
    pub fn get(&self, task_id: &TaskId) -> Result<Option<String>> {
        let key = format!("session:{}", task_id.0);
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(SESSION_TABLE)?;
        match table.get(key.as_str())? {
            Some(guard) => Ok(Some(guard.value().to_string())),
            None => Ok(None),
        }
    }

    /// Remove the stored session ID for a task.
    pub fn remove(&self, task_id: &TaskId) -> Result<bool> {
        let key = format!("session:{}", task_id.0);
        let write_txn = self.db.begin_write()?;
        let removed = {
            let mut table = write_txn.open_table(SESSION_TABLE)?;
            table.remove(key.as_str())?.is_some()
        };
        write_txn.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::task::TaskId;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        crate::open_db(&dir.path().join("test.redb")).unwrap()
    }

    #[test]
    fn save_and_get_session() {
        let db = test_db();
        let store = SessionStore::new(&db);
        let task_id = TaskId(42);

        store.save(&task_id, "ses-abc123").unwrap();
        let session = store.get(&task_id).unwrap();
        assert_eq!(session.as_deref(), Some("ses-abc123"));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = test_db();
        let store = SessionStore::new(&db);
        assert!(store.get(&TaskId(999)).unwrap().is_none());
    }

    #[test]
    fn save_overwrites_existing() {
        let db = test_db();
        let store = SessionStore::new(&db);
        let task_id = TaskId(1);

        store.save(&task_id, "ses-first").unwrap();
        store.save(&task_id, "ses-second").unwrap();

        let session = store.get(&task_id).unwrap();
        assert_eq!(session.as_deref(), Some("ses-second"));
    }

    #[test]
    fn remove_session() {
        let db = test_db();
        let store = SessionStore::new(&db);
        let task_id = TaskId(7);

        store.save(&task_id, "ses-remove").unwrap();
        let removed = store.remove(&task_id).unwrap();
        assert!(removed);
        assert!(store.get(&task_id).unwrap().is_none());

        // Removing again returns false
        let removed_again = store.remove(&task_id).unwrap();
        assert!(!removed_again);
    }
}
