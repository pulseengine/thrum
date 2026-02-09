//! End-to-end integration test exercising the full task lifecycle
//! through the store layer — the same state transitions the autonomous
//! loop performs, without requiring actual Claude sessions.

use thrum_core::task::*;
use thrum_db::gate_store::GateStore;
use thrum_db::task_store::TaskStore;

fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    thrum_db::open_db(&dir.path().join("lifecycle.redb")).unwrap()
}

/// Helper: build a passing gate report.
fn passing_gate(level: GateLevel) -> GateReport {
    GateReport {
        level,
        checks: vec![CheckResult {
            name: "test_check".into(),
            passed: true,
            stdout: "all good".into(),
            stderr: String::new(),
            exit_code: 0,
        }],
        passed: true,
        duration_secs: 1.0,
    }
}

/// Helper: build a failing gate report.
fn failing_gate(level: GateLevel) -> GateReport {
    GateReport {
        level,
        checks: vec![CheckResult {
            name: "test_check".into(),
            passed: false,
            stdout: String::new(),
            stderr: "assertion failed".into(),
            exit_code: 1,
        }],
        passed: false,
        duration_secs: 0.5,
    }
}

/// Full happy path: Pending → Implementing → Reviewing → AwaitingApproval → Approved → Merged.
#[test]
fn happy_path_lifecycle() {
    let db = test_db();
    let tasks = TaskStore::new(&db);
    let gates = GateStore::new(&db);

    // Step 1: Create task (Pending)
    let mut task = tasks
        .insert(Task::new(
            RepoName::new("loom"),
            "Add i32.popcnt support".into(),
            "Implement popcnt for the ISLE pipeline".into(),
        ))
        .unwrap();
    assert_eq!(task.id.0, 1);
    assert_eq!(task.status.label(), "pending");

    // Step 2: Start implementation
    let branch = task.branch_name();
    assert!(branch.starts_with("auto/TASK-0001/loom/"));

    task.status = TaskStatus::Implementing {
        branch: branch.clone(),
        started_at: chrono::Utc::now(),
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Verify it's no longer in the pending queue
    assert!(tasks.next_pending(None).unwrap().is_none());

    // Step 3: Gate 1 passes
    let gate1 = passing_gate(GateLevel::Quality);
    gates.store(&task.id, &gate1).unwrap();

    // Step 4: Review
    task.status = TaskStatus::Reviewing {
        reviewer_output: "LGTM, proof obligations met".into(),
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Step 5: Gate 2 passes
    let gate2 = passing_gate(GateLevel::Proof);
    gates.store(&task.id, &gate2).unwrap();

    // Step 6: Await approval
    let summary = CheckpointSummary {
        diff_summary: "+50 -10 lines".into(),
        reviewer_output: "LGTM".into(),
        gate1_report: gate1,
        gate2_report: Some(gate2),
    };
    task.status = TaskStatus::AwaitingApproval { summary };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();
    assert!(task.status.needs_human());

    // Verify it shows up in awaiting_approval
    let awaiting = tasks.awaiting_approval().unwrap();
    assert_eq!(awaiting.len(), 1);
    assert_eq!(awaiting[0].id.0, 1);

    // Step 7: Approve
    task.status = TaskStatus::Approved;
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Verify it shows up in next_approved
    let approved = tasks.next_approved(None).unwrap().unwrap();
    assert_eq!(approved.id.0, 1);

    // Step 8: Integration
    task.status = TaskStatus::Integrating;
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Gate 3 passes
    let gate3 = passing_gate(GateLevel::Integration);
    gates.store(&task.id, &gate3).unwrap();

    // Step 9: Merge
    task.status = TaskStatus::Merged {
        commit_sha: "abc123def456".into(),
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();
    assert!(task.status.is_terminal());

    // Verify all gate reports stored
    let all_gates = gates.get_all_for_task(&task.id).unwrap();
    assert_eq!(all_gates.len(), 3);

    // Verify final counts
    let counts = tasks.status_counts().unwrap();
    assert_eq!(counts.get("merged"), Some(&1));
}

/// Retry path: Gate1 fails, retries under limit, then succeeds.
#[test]
fn gate1_failure_retry() {
    let db = test_db();
    let tasks = TaskStore::new(&db);

    let mut task = tasks
        .insert(Task::new(
            RepoName::new("synth"),
            "Fix ARM encoding".into(),
            "Fix AAPCS calling convention".into(),
        ))
        .unwrap();

    // Move to implementing
    task.status = TaskStatus::Implementing {
        branch: task.branch_name(),
        started_at: chrono::Utc::now(),
    };
    tasks.update(&task).unwrap();

    // Gate 1 fails
    let gate1 = failing_gate(GateLevel::Quality);
    task.status = TaskStatus::Gate1Failed { report: gate1 };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Should appear in retryable failures
    assert!(task.can_retry());
    let retryable = tasks.retryable_failures(None).unwrap();
    assert_eq!(retryable.len(), 1);

    // Retry: bump count and move back to pending
    task.retry_count += 1;
    task.status = TaskStatus::Pending;
    tasks.update(&task).unwrap();

    // Now shows up in pending queue again
    let next = tasks.next_pending(None).unwrap().unwrap();
    assert_eq!(next.id.0, task.id.0);
    assert_eq!(next.retry_count, 1);
}

/// Retry limit reached: after MAX_RETRIES, can_retry() returns false.
#[test]
fn retry_limit() {
    let db = test_db();
    let tasks = TaskStore::new(&db);

    let mut task = tasks
        .insert(Task::new(
            RepoName::new("meld"),
            "Broken task".into(),
            "Always fails".into(),
        ))
        .unwrap();

    // Exhaust retries
    task.retry_count = MAX_RETRIES;
    task.status = TaskStatus::Gate1Failed {
        report: failing_gate(GateLevel::Quality),
    };
    tasks.update(&task).unwrap();

    assert!(!task.can_retry());
    let retryable = tasks.retryable_failures(None).unwrap();
    assert!(retryable.is_empty());
}

/// Rejection path: AwaitingApproval → Rejected with feedback.
#[test]
fn rejection_path() {
    let db = test_db();
    let tasks = TaskStore::new(&db);

    let mut task = tasks
        .insert(Task::new(
            RepoName::new("loom"),
            "Bad implementation".into(),
            "desc".into(),
        ))
        .unwrap();

    // Fast-forward to awaiting approval
    let summary = CheckpointSummary {
        diff_summary: "diff".into(),
        reviewer_output: "needs work".into(),
        gate1_report: passing_gate(GateLevel::Quality),
        gate2_report: None,
    };
    task.status = TaskStatus::AwaitingApproval { summary };
    tasks.update(&task).unwrap();

    // Reject
    task.status = TaskStatus::Rejected {
        feedback: "Proof obligations not met for REQ-LOOM-042".into(),
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    let fetched = tasks.get(&task.id).unwrap().unwrap();
    assert_eq!(fetched.status.label(), "rejected");
}

/// Multi-repo queue ordering: tasks are returned FIFO by ID.
#[test]
fn multi_repo_fifo() {
    let db = test_db();
    let tasks = TaskStore::new(&db);

    let t1 = tasks
        .insert(Task::new(
            RepoName::new("loom"),
            "Loom task 1".into(),
            "d".into(),
        ))
        .unwrap();
    let _t2 = tasks
        .insert(Task::new(
            RepoName::new("synth"),
            "Synth task".into(),
            "d".into(),
        ))
        .unwrap();
    let _t3 = tasks
        .insert(Task::new(
            RepoName::new("loom"),
            "Loom task 2".into(),
            "d".into(),
        ))
        .unwrap();

    // Global next: should be first inserted
    let next = tasks.next_pending(None).unwrap().unwrap();
    assert_eq!(next.id.0, t1.id.0);

    // Filter by repo
    let synth_next = tasks
        .next_pending(Some(&RepoName::new("synth")))
        .unwrap()
        .unwrap();
    assert_eq!(synth_next.title, "Synth task");

    // Counts
    let counts = tasks.status_counts().unwrap();
    assert_eq!(counts.get("pending"), Some(&3));
}

/// Gate 2 failure also triggers retry.
#[test]
fn gate2_failure_retry() {
    let db = test_db();
    let tasks = TaskStore::new(&db);

    let mut task = tasks
        .insert(Task::new(
            RepoName::new("loom"),
            "Proof failure".into(),
            "Z3 can't verify".into(),
        ))
        .unwrap();

    task.status = TaskStatus::Gate2Failed {
        report: failing_gate(GateLevel::Proof),
    };
    tasks.update(&task).unwrap();

    let retryable = tasks.retryable_failures(None).unwrap();
    assert_eq!(retryable.len(), 1);
    assert_eq!(retryable[0].status.label(), "gate2-failed");
}

/// Claimed status: task claimed by agent, transitions to Implementing.
#[test]
fn claimed_status_lifecycle() {
    let db = test_db();
    let tasks = TaskStore::new(&db);

    let mut task = tasks
        .insert(Task::new(
            RepoName::new("loom"),
            "Claimable task".into(),
            "desc".into(),
        ))
        .unwrap();
    assert_eq!(task.status.label(), "pending");

    // Claim the task
    task.status = TaskStatus::Claimed {
        agent_id: "agent-1-loom-TASK-0001".into(),
        claimed_at: chrono::Utc::now(),
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    let fetched = tasks.get(&task.id).unwrap().unwrap();
    assert_eq!(fetched.status.label(), "claimed");

    // Claimed task should NOT appear in next_pending
    assert!(tasks.next_pending(None).unwrap().is_none());

    // Transition to Implementing
    task.status = TaskStatus::Implementing {
        branch: task.branch_name(),
        started_at: chrono::Utc::now(),
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    let fetched = tasks.get(&task.id).unwrap().unwrap();
    assert_eq!(fetched.status.label(), "implementing");
}

/// Spec-based task preserves spec through serialization roundtrip.
#[test]
fn spec_roundtrip() {
    use thrum_core::spec::Spec;

    let db = test_db();
    let tasks = TaskStore::new(&db);

    let mut task = Task::new(RepoName::new("loom"), "Spec task".into(), "d".into());
    task.spec = Some(Spec {
        title: "Add popcnt".into(),
        context: "Needed for crypto workloads".into(),
        ..Default::default()
    });

    let inserted = tasks.insert(task).unwrap();
    let fetched = tasks.get(&inserted.id).unwrap().unwrap();

    assert!(fetched.spec.is_some());
    assert_eq!(fetched.spec.unwrap().title, "Add popcnt");
}
