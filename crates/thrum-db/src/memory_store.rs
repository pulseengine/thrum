use anyhow::Result;
use redb::{Database, ReadableTable, TableDefinition};
use thrum_core::memory::{MemoryEntry, MemoryId};
use thrum_core::task::RepoName;

/// Memory table: MemoryId string -> JSON-serialized MemoryEntry.
pub const MEMORY_TABLE: TableDefinition<&str, &str> = TableDefinition::new("memory");

pub struct MemoryStore<'a> {
    db: &'a Database,
}

impl<'a> MemoryStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Store a memory entry (upserts by ID).
    pub fn store(&self, entry: &MemoryEntry) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(MEMORY_TABLE)?;
            let json = serde_json::to_string(entry)?;
            table.insert(entry.id.0.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Get a memory entry by ID.
    pub fn get(&self, id: &MemoryId) -> Result<Option<MemoryEntry>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(MEMORY_TABLE)?;
        match table.get(id.0.as_str())? {
            Some(guard) => {
                let entry: MemoryEntry = serde_json::from_str(guard.value())?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    /// Query memories for a repo, sorted by relevance_score descending.
    pub fn query_for_task(&self, repo: &RepoName, limit: usize) -> Result<Vec<MemoryEntry>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(MEMORY_TABLE)?;
        let mut entries = Vec::new();

        let iter = table.iter()?;
        for item in iter {
            let (_, value) = item?;
            let entry: MemoryEntry = serde_json::from_str(value.value())?;
            if &entry.repo == repo {
                entries.push(entry);
            }
        }

        // Sort by relevance descending
        entries.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries.truncate(limit);
        Ok(entries)
    }

    /// Apply decay to all memory entries. Returns count of entries decayed.
    pub fn decay_all(&self, half_life_hours: f64) -> Result<u32> {
        let write_txn = self.db.begin_write()?;
        let count;
        {
            let mut table = write_txn.open_table(MEMORY_TABLE)?;
            let mut updates = Vec::new();

            {
                let iter = table.iter()?;
                for item in iter {
                    let (key, value) = item?;
                    let mut entry: MemoryEntry = serde_json::from_str(value.value())?;
                    entry.decay_score(half_life_hours);
                    updates.push((key.value().to_string(), entry));
                }
            }

            count = updates.len() as u32;
            for (key, entry) in &updates {
                let json = serde_json::to_string(entry)?;
                table.insert(key.as_str(), json.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(count)
    }

    /// Prune entries below a minimum relevance score. Returns count pruned.
    pub fn prune_below(&self, min_score: f64) -> Result<u32> {
        let write_txn = self.db.begin_write()?;
        let count;
        {
            let mut table = write_txn.open_table(MEMORY_TABLE)?;
            let mut to_remove = Vec::new();

            {
                let iter = table.iter()?;
                for item in iter {
                    let (key, value) = item?;
                    let entry: MemoryEntry = serde_json::from_str(value.value())?;
                    if entry.relevance_score < min_score {
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

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::memory::MemoryCategory;
    use thrum_core::task::TaskId;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        crate::open_db(&dir.path().join("test.redb")).unwrap()
    }

    #[test]
    fn store_and_get() {
        let db = test_db();
        let store = MemoryStore::new(&db);

        let entry = MemoryEntry::new(
            TaskId(1),
            RepoName::new("loom"),
            MemoryCategory::Error {
                error_type: "compile".into(),
            },
            "missing trait impl".into(),
        );
        let id = entry.id.clone();

        store.store(&entry).unwrap();
        let fetched = store.get(&id).unwrap().unwrap();
        assert_eq!(fetched.content, "missing trait impl");
    }

    #[test]
    fn query_by_repo() {
        let db = test_db();
        let store = MemoryStore::new(&db);

        store
            .store(&MemoryEntry::new(
                TaskId(1),
                RepoName::new("loom"),
                MemoryCategory::Pattern {
                    pattern_name: "test".into(),
                },
                "loom pattern".into(),
            ))
            .unwrap();

        store
            .store(&MemoryEntry::new(
                TaskId(2),
                RepoName::new("synth"),
                MemoryCategory::Pattern {
                    pattern_name: "test".into(),
                },
                "synth pattern".into(),
            ))
            .unwrap();

        let results = store.query_for_task(&RepoName::new("loom"), 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "loom pattern");
    }

    #[test]
    fn prune_below_threshold() {
        let db = test_db();
        let store = MemoryStore::new(&db);

        let mut entry = MemoryEntry::new(
            TaskId(1),
            RepoName::new("loom"),
            MemoryCategory::Context {
                scope: "test".into(),
            },
            "low relevance".into(),
        );
        entry.relevance_score = 0.01;
        store.store(&entry).unwrap();

        store
            .store(&MemoryEntry::new(
                TaskId(2),
                RepoName::new("loom"),
                MemoryCategory::Context {
                    scope: "test".into(),
                },
                "high relevance".into(),
            ))
            .unwrap();

        let pruned = store.prune_below(0.1).unwrap();
        assert_eq!(pruned, 1);

        let remaining = store.query_for_task(&RepoName::new("loom"), 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "high relevance");
    }
}
