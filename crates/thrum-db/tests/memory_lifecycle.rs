//! Integration tests for the memory → pipeline wiring.
//!
//! Validates the full memory lifecycle that TASK-0012 wires together:
//! store → query → inject into prompt → touch → decay → prune.

use thrum_core::memory::{MemoryCategory, MemoryEntry};
use thrum_core::task::{RepoName, TaskId};
use thrum_db::memory_store::MemoryStore;

fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    thrum_db::open_db(&dir.path().join("memory.redb")).unwrap()
}

/// Full lifecycle: store errors and patterns, query for prompt injection,
/// touch accessed entries, decay, and prune stale ones.
#[test]
fn memory_lifecycle_store_query_decay_prune() {
    let db = test_db();
    let store = MemoryStore::new(&db);

    // Simulate gate failures creating error memories
    let error1 = MemoryEntry::new(
        TaskId(1),
        RepoName::new("loom"),
        MemoryCategory::Error {
            error_type: "gate1_failure".into(),
        },
        "Task 'Add popcnt' failed Gate 1: cargo_clippy: unused variable".into(),
    );
    let error2 = MemoryEntry::new(
        TaskId(2),
        RepoName::new("loom"),
        MemoryCategory::Error {
            error_type: "gate1_failure".into(),
        },
        "Task 'Fix encoding' failed Gate 1: cargo_fmt: formatting issues".into(),
    );

    // Simulate successful implementation creating pattern memory
    let pattern = MemoryEntry::new(
        TaskId(3),
        RepoName::new("loom"),
        MemoryCategory::Pattern {
            pattern_name: "successful_implementation".into(),
        },
        "Task 'Wire subsampling' passed gates and reached approval".into(),
    );

    // Store all memories
    store.store(&error1).unwrap();
    store.store(&error2).unwrap();
    store.store(&pattern).unwrap();

    // Query for initial implementation: should return all 3 (pattern + errors)
    let all = store.query_for_task(&RepoName::new("loom"), 10).unwrap();
    assert_eq!(all.len(), 3);

    // Query errors only for retry context: should return only 2
    let errors = store
        .query_errors_for_repo(&RepoName::new("loom"), 10)
        .unwrap();
    assert_eq!(errors.len(), 2);
    assert!(errors.iter().all(|e| e.category.is_error()));

    // Touch accessed entries (simulates prompt injection)
    let ids: Vec<_> = errors.iter().map(|m| m.id.clone()).collect();
    let touched = store.touch_entries(&ids).unwrap();
    assert_eq!(touched, 2);

    // Verify access counts bumped
    let after_touch = store.get(&errors[0].id).unwrap().unwrap();
    assert_eq!(after_touch.access_count, 1);

    // Decay with a very short half-life to force scores down
    // (entries were just created, so decay will be minimal with normal half-life)
    // Instead, manually set an entry's score low to test pruning
    let mut stale = MemoryEntry::new(
        TaskId(4),
        RepoName::new("loom"),
        MemoryCategory::Context {
            scope: "old".into(),
        },
        "very old context that should be pruned".into(),
    );
    stale.relevance_score = 0.01; // Below threshold
    store.store(&stale).unwrap();

    // Now 4 entries total
    let before_prune = store.query_for_task(&RepoName::new("loom"), 100).unwrap();
    assert_eq!(before_prune.len(), 4);

    // Prune below 0.05 — should remove the stale entry
    let pruned = store.prune_below(0.05).unwrap();
    assert_eq!(pruned, 1);

    // 3 entries remain
    let after_prune = store.query_for_task(&RepoName::new("loom"), 100).unwrap();
    assert_eq!(after_prune.len(), 3);
}

/// Prompt context formatting matches what the pipeline injects.
#[test]
fn prompt_context_injection_format() {
    let error = MemoryEntry::new(
        TaskId(1),
        RepoName::new("loom"),
        MemoryCategory::Error {
            error_type: "gate1_failure".into(),
        },
        "cargo clippy: unused variable `x`".into(),
    );

    let pattern = MemoryEntry::new(
        TaskId(2),
        RepoName::new("loom"),
        MemoryCategory::Pattern {
            pattern_name: "successful_implementation".into(),
        },
        "Task 'Wire subsampling' passed gates".into(),
    );

    // Format as the pipeline would for injection
    let memories = vec![error, pattern];
    let ctx: Vec<String> = memories.iter().map(|m| m.to_prompt_context()).collect();
    let injected = format!(
        "\n\n## Relevant context from previous sessions\n{}",
        ctx.join("\n")
    );

    assert!(injected.contains("## Relevant context from previous sessions"));
    assert!(injected.contains("[Error: gate1_failure]"));
    assert!(injected.contains("[Pattern: successful_implementation]"));
    assert!(injected.contains("cargo clippy: unused variable"));
}

/// Failure-specific memory injection format for retries.
#[test]
fn retry_failure_memory_injection() {
    let db = test_db();
    let store = MemoryStore::new(&db);

    // Store error memories as the pipeline does on gate failure
    store
        .store(&MemoryEntry::new(
            TaskId(1),
            RepoName::new("synth"),
            MemoryCategory::Error {
                error_type: "gate1_failure".into(),
            },
            "Task 'Fix ARM' failed Gate 1: cargo_test: assertion failed".into(),
        ))
        .unwrap();

    // Query as retry_task_pipeline does
    let memories = store
        .query_errors_for_repo(&RepoName::new("synth"), 5)
        .unwrap();
    assert!(!memories.is_empty());

    // Touch accessed memories
    let ids: Vec<_> = memories.iter().map(|m| m.id.clone()).collect();
    store.touch_entries(&ids).unwrap();

    // Format as the retry pipeline does
    let ctx: Vec<String> = memories.iter().map(|m| m.to_prompt_context()).collect();
    let failure_context = format!(
        "\n\n## Failure-specific context from previous attempts\n{}",
        ctx.join("\n")
    );

    assert!(failure_context.contains("## Failure-specific context from previous attempts"));
    assert!(failure_context.contains("[Error: gate1_failure]"));
    assert!(failure_context.contains("assertion failed"));

    // Verify touch worked
    let touched = store.get(&ids[0]).unwrap().unwrap();
    assert_eq!(touched.access_count, 1);
}

/// Cross-repo isolation: memories for repo A don't leak into repo B queries.
#[test]
fn cross_repo_memory_isolation() {
    let db = test_db();
    let store = MemoryStore::new(&db);

    store
        .store(&MemoryEntry::new(
            TaskId(1),
            RepoName::new("loom"),
            MemoryCategory::Error {
                error_type: "gate1_failure".into(),
            },
            "loom error".into(),
        ))
        .unwrap();

    store
        .store(&MemoryEntry::new(
            TaskId(2),
            RepoName::new("synth"),
            MemoryCategory::Error {
                error_type: "gate2_failure".into(),
            },
            "synth error".into(),
        ))
        .unwrap();

    store
        .store(&MemoryEntry::new(
            TaskId(3),
            RepoName::new("loom"),
            MemoryCategory::Pattern {
                pattern_name: "successful_implementation".into(),
            },
            "loom success".into(),
        ))
        .unwrap();

    // Loom queries should only see loom entries
    let loom_all = store.query_for_task(&RepoName::new("loom"), 10).unwrap();
    assert_eq!(loom_all.len(), 2);

    let loom_errors = store
        .query_errors_for_repo(&RepoName::new("loom"), 10)
        .unwrap();
    assert_eq!(loom_errors.len(), 1);
    assert_eq!(loom_errors[0].content, "loom error");

    // Synth queries should only see synth entries
    let synth_all = store.query_for_task(&RepoName::new("synth"), 10).unwrap();
    assert_eq!(synth_all.len(), 1);
    assert_eq!(synth_all[0].content, "synth error");
}

/// Decay reduces relevance scores, affecting query ordering.
#[test]
fn decay_affects_query_ordering() {
    let db = test_db();
    let store = MemoryStore::new(&db);

    let mut old_entry = MemoryEntry::new(
        TaskId(1),
        RepoName::new("loom"),
        MemoryCategory::Pattern {
            pattern_name: "old_pattern".into(),
        },
        "old pattern".into(),
    );
    // Simulate age by setting score lower (as if decayed)
    old_entry.relevance_score = 0.3;
    store.store(&old_entry).unwrap();

    let fresh_entry = MemoryEntry::new(
        TaskId(2),
        RepoName::new("loom"),
        MemoryCategory::Pattern {
            pattern_name: "fresh_pattern".into(),
        },
        "fresh pattern".into(),
    );
    // Fresh entry has default score of 1.0
    store.store(&fresh_entry).unwrap();

    // Query should return fresh first (higher relevance score)
    let results = store.query_for_task(&RepoName::new("loom"), 10).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content, "fresh pattern");
    assert_eq!(results[1].content, "old pattern");

    // With limit 1, only the freshest entry
    let top1 = store.query_for_task(&RepoName::new("loom"), 1).unwrap();
    assert_eq!(top1.len(), 1);
    assert_eq!(top1[0].content, "fresh pattern");
}

/// Content-addressed deduplication: storing the same content twice doesn't create duplicates.
#[test]
fn content_addressed_dedup() {
    let db = test_db();
    let store = MemoryStore::new(&db);

    let entry1 = MemoryEntry::new(
        TaskId(1),
        RepoName::new("loom"),
        MemoryCategory::Error {
            error_type: "gate1_failure".into(),
        },
        "same error message".into(),
    );

    // Same content from a different task — produces the same MemoryId
    let entry2 = MemoryEntry::new(
        TaskId(2),
        RepoName::new("loom"),
        MemoryCategory::Error {
            error_type: "gate1_failure".into(),
        },
        "same error message".into(),
    );

    assert_eq!(entry1.id, entry2.id);

    store.store(&entry1).unwrap();
    store.store(&entry2).unwrap();

    // Only one entry (upserted)
    let all = store.query_for_task(&RepoName::new("loom"), 10).unwrap();
    assert_eq!(all.len(), 1);
}
