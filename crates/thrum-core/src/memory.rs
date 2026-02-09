use crate::task::{RepoName, TaskId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Content-addressed memory entry with semantic decay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub task_id: TaskId,
    pub repo: RepoName,
    pub category: MemoryCategory,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub access_count: u32,
    pub last_accessed: DateTime<Utc>,
    pub relevance_score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub String);

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemoryCategory {
    Error { error_type: String },
    Pattern { pattern_name: String },
    Decision { alternatives: Vec<String> },
    Context { scope: String },
}

impl MemoryEntry {
    /// Create a new memory entry with a content-derived ID.
    pub fn new(task_id: TaskId, repo: RepoName, category: MemoryCategory, content: String) -> Self {
        let id = MemoryId(content_hash(&content));
        let now = Utc::now();
        Self {
            id,
            task_id,
            repo,
            category,
            content,
            created_at: now,
            access_count: 0,
            last_accessed: now,
            relevance_score: 1.0,
        }
    }

    /// Apply exponential decay to the relevance score.
    pub fn decay_score(&mut self, half_life_hours: f64) {
        let hours_since = (Utc::now() - self.last_accessed).num_seconds() as f64 / 3600.0;
        self.relevance_score *= 0.5_f64.powf(hours_since / half_life_hours);
    }

    /// Bump access count and last_accessed timestamp.
    pub fn touch(&mut self) {
        self.access_count += 1;
        self.last_accessed = Utc::now();
    }

    /// Format this memory entry for injection into a prompt.
    pub fn to_prompt_context(&self) -> String {
        let cat = match &self.category {
            MemoryCategory::Error { error_type } => format!("[Error: {error_type}]"),
            MemoryCategory::Pattern { pattern_name } => format!("[Pattern: {pattern_name}]"),
            MemoryCategory::Decision { alternatives } => {
                format!("[Decision, alternatives: {}]", alternatives.join(", "))
            }
            MemoryCategory::Context { scope } => format!("[Context: {scope}]"),
        };
        format!("{cat} {}", self.content)
    }
}

/// Simple content hash using first 16 hex chars of a basic hash.
fn content_hash(content: &str) -> String {
    // Use a simple hash since we don't need cryptographic strength here
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for byte in content.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    let hash2: u64 = hash.wrapping_mul(0x517cc1b727220a95);
    format!("{hash:016x}{hash2:016x}")[..16].to_string()
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// content_hash is deterministic: same content always produces the same hash.
        #[test]
        fn content_hash_deterministic(content in ".*") {
            let h1 = content_hash(&content);
            let h2 = content_hash(&content);
            prop_assert_eq!(h1, h2);
        }

        /// Different content should (almost always) produce different hashes.
        /// We test with two distinct non-empty strings.
        #[test]
        fn content_hash_different_inputs(a in ".{1,50}", b in ".{1,50}") {
            prop_assume!(a != b);
            let h1 = content_hash(&a);
            let h2 = content_hash(&b);
            // FNV collisions are theoretically possible but astronomically unlikely
            // for short strings. We accept the rare failure.
            prop_assert_ne!(h1, h2);
        }

        /// touch() always increases access_count by exactly 1.
        #[test]
        fn touch_increases_access_count(initial_count in 0u32..1000) {
            let mut entry = MemoryEntry::new(
                TaskId(1),
                RepoName::new("test"),
                MemoryCategory::Context { scope: "test".into() },
                "test content".into(),
            );
            entry.access_count = initial_count;
            entry.touch();
            prop_assert_eq!(entry.access_count, initial_count + 1);
        }

        /// decay_score produces a value <= the current relevance_score
        /// (since the decay factor is always in [0, 1]).
        #[test]
        fn decay_score_monotonically_decreasing(
            initial_score in 0.0f64..=1.0,
            half_life in 0.1f64..100.0,
        ) {
            let mut entry = MemoryEntry::new(
                TaskId(1),
                RepoName::new("test"),
                MemoryCategory::Pattern { pattern_name: "test".into() },
                "test content".into(),
            );
            entry.relevance_score = initial_score;
            // Set last_accessed to now so decay is minimal but non-negative
            entry.last_accessed = chrono::Utc::now();
            let before = entry.relevance_score;
            entry.decay_score(half_life);
            prop_assert!(entry.relevance_score <= before + f64::EPSILON,
                "decay should not increase score: {} -> {}", before, entry.relevance_score);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_entry_creation() {
        let entry = MemoryEntry::new(
            TaskId(1),
            RepoName::new("loom"),
            MemoryCategory::Error {
                error_type: "compile".into(),
            },
            "Failed to compile due to missing trait impl".into(),
        );
        assert_eq!(entry.relevance_score, 1.0);
        assert_eq!(entry.access_count, 0);
        assert!(!entry.id.0.is_empty());
    }

    #[test]
    fn memory_touch_bumps_access() {
        let mut entry = MemoryEntry::new(
            TaskId(1),
            RepoName::new("loom"),
            MemoryCategory::Pattern {
                pattern_name: "builder".into(),
            },
            "Use builder pattern for config".into(),
        );
        assert_eq!(entry.access_count, 0);
        entry.touch();
        assert_eq!(entry.access_count, 1);
        entry.touch();
        assert_eq!(entry.access_count, 2);
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        let h3 = content_hash("different content");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn prompt_context_formatting() {
        let entry = MemoryEntry::new(
            TaskId(1),
            RepoName::new("synth"),
            MemoryCategory::Error {
                error_type: "test_failure".into(),
            },
            "cargo test failed on ARM encoding".into(),
        );
        let ctx = entry.to_prompt_context();
        assert!(ctx.contains("[Error: test_failure]"));
        assert!(ctx.contains("ARM encoding"));
    }
}
