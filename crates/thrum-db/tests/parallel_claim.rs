//! Concurrency tests for atomic task claiming.
//!
//! Validates that the claim_next() mechanism prevents double-dispatch
//! even under concurrent access from multiple threads.

use std::collections::HashSet;
use std::sync::Arc;
use thrum_core::task::*;
use thrum_db::task_store::{ClaimCategory, TaskStore};

fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("parallel.redb");
    let db = thrum_db::open_db(&path).unwrap();
    // Leak the tempdir so the file outlives the test
    std::mem::forget(dir);
    db
}

/// 10 threads each try to claim from a pool of 10 tasks using a shared
/// Arc<Database>. redb enforces single-writer transactions, so even with
/// concurrent callers, each task should be claimed exactly once.
#[test]
fn concurrent_claims_no_double_dispatch() {
    let db = Arc::new(test_db());

    // Insert 10 pending tasks
    {
        let store = TaskStore::new(&db);
        for i in 0..10 {
            store
                .insert(Task::new(
                    RepoName::new("loom"),
                    format!("Task {i}"),
                    "d".into(),
                ))
                .unwrap();
        }
    }

    // Spawn 10 threads, each sharing the same Arc<Database>
    let mut handles = Vec::new();
    for thread_id in 0..10 {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            let store = TaskStore::new(&db);
            let mut claimed = Vec::new();

            loop {
                let agent_id = format!("thread-{thread_id}");
                match store.claim_next(&agent_id, ClaimCategory::Pending, None) {
                    Ok(Some(task)) => claimed.push(task.id.0),
                    Ok(None) => break,
                    Err(_) => {
                        // redb serializes writers; transient errors should be retried
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                }
            }
            claimed
        }));
    }

    let mut all_claimed: Vec<i64> = Vec::new();
    for handle in handles {
        all_claimed.extend(handle.join().unwrap());
    }

    // Verify: all 10 tasks claimed, no duplicates
    let unique: HashSet<i64> = all_claimed.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        all_claimed.len(),
        "duplicate task claims detected: {all_claimed:?}"
    );
    assert_eq!(unique.len(), 10, "not all tasks were claimed");
}

/// Retryable tasks are claimed before approved, which are claimed before pending.
#[test]
fn claim_category_priority() {
    let db = test_db();
    let store = TaskStore::new(&db);

    // Insert a pending task
    store
        .insert(Task::new(
            RepoName::new("loom"),
            "Pending task".into(),
            "d".into(),
        ))
        .unwrap();

    // Insert an approved task
    let mut approved = store
        .insert(Task::new(
            RepoName::new("synth"),
            "Approved task".into(),
            "d".into(),
        ))
        .unwrap();
    approved.status = TaskStatus::Approved;
    approved.updated_at = chrono::Utc::now();
    store.update(&approved).unwrap();

    // Insert a retryable failed task
    let mut failed = store
        .insert(Task::new(
            RepoName::new("meld"),
            "Failed task".into(),
            "d".into(),
        ))
        .unwrap();
    failed.status = TaskStatus::Gate1Failed {
        report: GateReport {
            level: GateLevel::Quality,
            checks: vec![CheckResult {
                name: "test".into(),
                passed: false,
                stdout: String::new(),
                stderr: "fail".into(),
                exit_code: 1,
            }],
            passed: false,
            duration_secs: 0.5,
        },
    };
    store.update(&failed).unwrap();

    // Priority 1: RetryableFailed should be claimed first
    let first = store
        .claim_next("agent", ClaimCategory::RetryableFailed, None)
        .unwrap();
    assert!(first.is_some());
    assert_eq!(first.unwrap().title, "Failed task");

    // Priority 2: Approved should be claimed next
    let second = store
        .claim_next("agent", ClaimCategory::Approved, None)
        .unwrap();
    assert!(second.is_some());
    assert_eq!(second.unwrap().title, "Approved task");

    // Priority 3: Pending should be claimed last
    let third = store
        .claim_next("agent", ClaimCategory::Pending, None)
        .unwrap();
    assert!(third.is_some());
    assert_eq!(third.unwrap().title, "Pending task");

    // Nothing left in any category
    assert!(
        store
            .claim_next("agent", ClaimCategory::RetryableFailed, None)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .claim_next("agent", ClaimCategory::Approved, None)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .claim_next("agent", ClaimCategory::Pending, None)
            .unwrap()
            .is_none()
    );
}
