use anyhow::Result;
use redb::{Database, TableDefinition};
use thrum_core::budget::BudgetTracker;

/// Single-key table storing the serialized BudgetTracker.
pub const BUDGET_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("budget");

/// Persistent budget store backed by redb.
///
/// Stores the entire `BudgetTracker` as a single serialized value,
/// updated atomically after each agent invocation records cost.
pub struct BudgetStore<'a> {
    db: &'a Database,
}

const BUDGET_KEY: &str = "tracker";

impl<'a> BudgetStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Load the budget tracker from the database.
    /// Returns `None` if no budget has been persisted yet.
    pub fn load(&self) -> Result<Option<BudgetTracker>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(BUDGET_TABLE) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        match table.get(BUDGET_KEY)? {
            Some(bytes) => {
                let tracker: BudgetTracker = serde_json::from_slice(bytes.value())?;
                Ok(Some(tracker))
            }
            None => Ok(None),
        }
    }

    /// Persist the budget tracker to the database.
    pub fn save(&self, tracker: &BudgetTracker) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(BUDGET_TABLE)?;
            let bytes = serde_json::to_vec(tracker)?;
            table.insert(BUDGET_KEY, bytes.as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Expose the database reference for callers that need it.
    pub fn db(&self) -> &Database {
        self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::budget::{BudgetEntry, SessionType};

    #[test]
    fn budget_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let db = crate::open_db(&db_path).unwrap();

        let store = BudgetStore::new(&db);

        // Initially empty
        assert!(store.load().unwrap().is_none());

        // Save a tracker with an entry
        let mut tracker = BudgetTracker::new(1000.0);
        tracker.record(BudgetEntry {
            task_id: 1,
            session_type: SessionType::Implementation,
            model: "opus-4".into(),
            input_tokens: 50_000,
            output_tokens: 10_000,
            estimated_cost_usd: 5.25,
            timestamp: chrono::Utc::now(),
        });

        store.save(&tracker).unwrap();

        // Reload and verify
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.ceiling_usd, 1000.0);
        assert_eq!(loaded.entries.len(), 1);
        assert!((loaded.total_spent() - 5.25).abs() < f64::EPSILON);
        assert!((loaded.remaining() - 994.75).abs() < f64::EPSILON);
    }
}
