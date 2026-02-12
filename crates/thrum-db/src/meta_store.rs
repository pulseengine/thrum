//! Key-value metadata store for engine state that doesn't fit elsewhere.
//!
//! Currently tracks the last-seen git commit per-repo so Thrum can detect
//! when the repository has advanced (e.g. manual commits, external merges)
//! and prompt for changelog review or re-validation.

use anyhow::Result;
use redb::{Database, TableDefinition};

/// redb table: key = metadata key string, value = metadata value string.
pub const META_TABLE: TableDefinition<&str, &str> = TableDefinition::new("meta");

/// Prefix for per-repo last-seen commit keys.
const LAST_COMMIT_PREFIX: &str = "last_commit:";

pub struct MetaStore<'a> {
    db: &'a Database,
}

impl<'a> MetaStore<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Get a raw metadata value by key.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(META_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_string()))
    }

    /// Set a raw metadata value.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(META_TABLE)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Get the last-seen git commit hash for a repo.
    pub fn last_commit(&self, repo: &str) -> Result<Option<String>> {
        let key = format!("{LAST_COMMIT_PREFIX}{repo}");
        self.get(&key)
    }

    /// Record the current git commit hash for a repo.
    /// Returns the previous commit hash if there was one.
    pub fn record_commit(&self, repo: &str, commit_sha: &str) -> Result<Option<String>> {
        let key = format!("{LAST_COMMIT_PREFIX}{repo}");
        let previous = self.get(&key)?;
        self.set(&key, commit_sha)?;
        Ok(previous)
    }

    /// Check if a repo has advanced beyond what Thrum last saw.
    /// Returns `Some((old, new))` if the repo moved, `None` if unchanged.
    pub fn check_repo_advanced(
        &self,
        repo: &str,
        current_sha: &str,
    ) -> Result<Option<(String, String)>> {
        match self.last_commit(repo)? {
            Some(last) if last != current_sha => Ok(Some((last, current_sha.to_string()))),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        crate::open_db(&dir.path().join("test.redb")).unwrap()
    }

    #[test]
    fn get_missing_returns_none() {
        let db = test_db();
        let store = MetaStore::new(&db);
        assert!(store.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn set_and_get_roundtrip() {
        let db = test_db();
        let store = MetaStore::new(&db);
        store.set("foo", "bar").unwrap();
        assert_eq!(store.get("foo").unwrap().as_deref(), Some("bar"));
    }

    #[test]
    fn record_commit_returns_previous() {
        let db = test_db();
        let store = MetaStore::new(&db);

        let prev = store.record_commit("thrum", "abc123").unwrap();
        assert!(prev.is_none());

        let prev = store.record_commit("thrum", "def456").unwrap();
        assert_eq!(prev.as_deref(), Some("abc123"));
    }

    #[test]
    fn check_repo_advanced_detects_change() {
        let db = test_db();
        let store = MetaStore::new(&db);

        // No prior commit — not "advanced"
        let result = store.check_repo_advanced("thrum", "abc123").unwrap();
        assert!(result.is_none());

        // Record the commit
        store.record_commit("thrum", "abc123").unwrap();

        // Same commit — not advanced
        let result = store.check_repo_advanced("thrum", "abc123").unwrap();
        assert!(result.is_none());

        // Different commit — advanced
        let result = store.check_repo_advanced("thrum", "def456").unwrap();
        assert_eq!(result, Some(("abc123".to_string(), "def456".to_string())));
    }

    #[test]
    fn per_repo_isolation() {
        let db = test_db();
        let store = MetaStore::new(&db);

        store.record_commit("loom", "aaa").unwrap();
        store.record_commit("synth", "bbb").unwrap();

        assert_eq!(store.last_commit("loom").unwrap().as_deref(), Some("aaa"));
        assert_eq!(store.last_commit("synth").unwrap().as_deref(), Some("bbb"));
        assert!(store.last_commit("meld").unwrap().is_none());
    }
}
