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

/// CI path: Approved → Integrating → AwaitingCI (push + PR) → Merged.
///
/// Exercises the CI-enabled flow where a task transitions through
/// the full pipeline including the AwaitingCI state that tracks
/// a pushed branch and created PR.
#[test]
fn ci_path_lifecycle() {
    let db = test_db();
    let tasks = TaskStore::new(&db);
    let gates = GateStore::new(&db);

    // Create and fast-forward to Approved
    let mut task = tasks
        .insert(Task::new(
            RepoName::new("loom"),
            "Add WASM SIMD support".into(),
            "Implement SIMD instructions for the WASM backend".into(),
        ))
        .unwrap();

    task.status = TaskStatus::Approved;
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Step 1: Integrating (Gate 3 runs)
    task.status = TaskStatus::Integrating;
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();
    assert_eq!(task.status.label(), "integrating");

    let gate3 = passing_gate(GateLevel::Integration);
    gates.store(&task.id, &gate3).unwrap();

    // Step 2: Push branch + create PR → AwaitingCI
    let branch = task.branch_name();
    let pr_number = 42u64;
    let pr_url = "https://github.com/org/loom/pull/42".to_string();

    task.status = TaskStatus::AwaitingCI {
        pr_number,
        pr_url: pr_url.clone(),
        branch: branch.clone(),
        started_at: chrono::Utc::now(),
        ci_attempts: 0,
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Verify AwaitingCI properties
    assert_eq!(task.status.label(), "awaiting-ci");
    assert!(task.status.is_awaiting_ci());
    assert!(!task.status.is_terminal());
    assert!(!task.status.needs_human());

    // Verify the PR metadata is stored and retrievable
    let fetched = tasks.get(&task.id).unwrap().unwrap();
    match &fetched.status {
        TaskStatus::AwaitingCI {
            pr_number: pn,
            pr_url: pu,
            branch: br,
            ci_attempts: ca,
            ..
        } => {
            assert_eq!(*pn, 42);
            assert_eq!(pu, "https://github.com/org/loom/pull/42");
            assert_eq!(br, &branch);
            assert_eq!(*ca, 0);
        }
        other => panic!("expected AwaitingCI, got {}", other.label()),
    }

    // Verify it shows up in status counts
    let counts = tasks.status_counts().unwrap();
    assert_eq!(counts.get("awaiting-ci"), Some(&1));

    // Verify it shows up when listing by status
    let ci_tasks = tasks.list(Some("awaiting-ci"), None).unwrap();
    assert_eq!(ci_tasks.len(), 1);
    assert_eq!(ci_tasks[0].id, task.id);

    // Step 3: CI passes → Merged
    task.status = TaskStatus::Merged {
        commit_sha: "deadbeef123456".into(),
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();
    assert!(task.status.is_terminal());
}

/// CI failure path: AwaitingCI → CIFailed after max retries.
///
/// Exercises the CI failure escalation path where the ci_fixer agent
/// exhausts its retries and the task escalates to human review.
#[test]
fn ci_failure_escalation() {
    let db = test_db();
    let tasks = TaskStore::new(&db);

    let mut task = tasks
        .insert(Task::new(
            RepoName::new("synth"),
            "Fix ARM NEON codegen".into(),
            "NEON intrinsics emit wrong opcodes".into(),
        ))
        .unwrap();

    // Fast-forward to AwaitingCI
    let branch = task.branch_name();
    task.status = TaskStatus::AwaitingCI {
        pr_number: 99,
        pr_url: "https://github.com/org/synth/pull/99".into(),
        branch: branch.clone(),
        started_at: chrono::Utc::now(),
        ci_attempts: 0,
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Simulate ci_fixer retry: increment attempts and stay in AwaitingCI
    task.status = TaskStatus::AwaitingCI {
        pr_number: 99,
        pr_url: "https://github.com/org/synth/pull/99".into(),
        branch: branch.clone(),
        started_at: chrono::Utc::now(),
        ci_attempts: 1,
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // Verify ci_attempts incremented
    let fetched = tasks.get(&task.id).unwrap().unwrap();
    match &fetched.status {
        TaskStatus::AwaitingCI { ci_attempts, .. } => {
            assert_eq!(*ci_attempts, 1);
        }
        other => panic!("expected AwaitingCI, got {}", other.label()),
    }

    // Escalate to CIFailed after max retries
    task.status = TaskStatus::CIFailed {
        pr_number: 99,
        pr_url: "https://github.com/org/synth/pull/99".into(),
        failure_summary: "test_neon_simd failed: wrong opcode for vaddq_f32".into(),
        ci_attempts: 4,
    };
    task.updated_at = chrono::Utc::now();
    tasks.update(&task).unwrap();

    // CIFailed needs human review
    assert!(task.status.needs_human());
    assert!(!task.status.is_terminal());
    assert_eq!(task.status.label(), "ci-failed");

    // Verify PR metadata preserved in CIFailed
    let fetched = tasks.get(&task.id).unwrap().unwrap();
    match &fetched.status {
        TaskStatus::CIFailed {
            pr_number,
            pr_url,
            failure_summary,
            ci_attempts,
        } => {
            assert_eq!(*pr_number, 99);
            assert_eq!(pr_url, "https://github.com/org/synth/pull/99");
            assert!(failure_summary.contains("wrong opcode"));
            assert_eq!(*ci_attempts, 4);
        }
        other => panic!("expected CIFailed, got {}", other.label()),
    }

    // Verify status counts
    let counts = tasks.status_counts().unwrap();
    assert_eq!(counts.get("ci-failed"), Some(&1));
}

/// CI integration is opt-in: when no [ci] section is present,
/// the repo config has ci = None, and `ci.enabled` defaults to true
/// only when explicitly specified.
#[test]
fn ci_config_opt_in() {
    use std::path::PathBuf;
    use thrum_core::repo::{CIConfig, RepoConfig};

    // Default repo config: no CI section → ci is None
    let config = RepoConfig {
        name: RepoName::new("my-project"),
        path: PathBuf::from("/tmp/test"),
        build_cmd: "cargo build".into(),
        test_cmd: "cargo test".into(),
        lint_cmd: "cargo clippy".into(),
        fmt_cmd: "cargo fmt --check".into(),
        verify_cmd: None,
        proofs_cmd: None,
        claude_md: None,
        safety_target: None,
        ci: None,
    };

    // When ci is None, CI is disabled (opt-in)
    let ci_enabled = config.ci.as_ref().is_some_and(|ci| ci.enabled);
    assert!(
        !ci_enabled,
        "CI should be disabled when no [ci] section is present"
    );

    // When ci section is present with defaults, CI is enabled
    let config_with_ci = RepoConfig {
        ci: Some(CIConfig::default()),
        ..config.clone()
    };
    let ci_enabled = config_with_ci.ci.as_ref().is_some_and(|ci| ci.enabled);
    assert!(
        ci_enabled,
        "CI should be enabled when [ci] section is present with defaults"
    );

    // When ci section is present but disabled, CI is off
    let config_disabled = RepoConfig {
        ci: Some(CIConfig {
            enabled: false,
            ..CIConfig::default()
        }),
        ..config
    };
    let ci_disabled = config_disabled.ci.as_ref().is_some_and(|ci| ci.enabled);
    assert!(!ci_disabled, "CI should be disabled when enabled = false");
}

/// CI config parses from TOML with [repo.ci] section.
#[test]
fn ci_config_toml_parsing() {
    use thrum_core::repo::ReposConfig;

    let toml_str = r#"
[[repo]]
name = "my-project"
path = "/tmp/test"
build_cmd = "cargo build"
test_cmd = "cargo test"
lint_cmd = "cargo clippy"
fmt_cmd = "cargo fmt --check"

[repo.ci]
enabled = true
poll_interval_secs = 30
max_ci_retries = 5
auto_merge = false
merge_strategy = "rebase"
"#;

    let config: ReposConfig = toml::from_str(toml_str).unwrap();
    let repo = &config.repo[0];

    let ci = repo.ci.as_ref().expect("CI config should be present");
    assert!(ci.enabled);
    assert_eq!(ci.poll_interval_secs, 30);
    assert_eq!(ci.max_ci_retries, 5);
    assert!(!ci.auto_merge);
    assert_eq!(ci.merge_strategy, "rebase");
}

/// CI config defaults work when [repo.ci] section has no fields.
#[test]
fn ci_config_defaults_from_toml() {
    use thrum_core::repo::ReposConfig;

    let toml_str = r#"
[[repo]]
name = "my-project"
path = "/tmp/test"
build_cmd = "cargo build"
test_cmd = "cargo test"
lint_cmd = "cargo clippy"
fmt_cmd = "cargo fmt --check"

[repo.ci]
"#;

    let config: ReposConfig = toml::from_str(toml_str).unwrap();
    let repo = &config.repo[0];

    let ci = repo.ci.as_ref().expect("CI config should be present");
    assert!(ci.enabled);
    assert_eq!(ci.poll_interval_secs, 60);
    assert_eq!(ci.max_ci_retries, 3);
    assert!(ci.auto_merge);
    assert_eq!(ci.merge_strategy, "squash");
}

/// CI disabled by default: repos without [ci] section skip CI.
#[test]
fn ci_disabled_by_default_in_toml() {
    use thrum_core::repo::ReposConfig;

    let toml_str = r#"
[[repo]]
name = "my-project"
path = "/tmp/test"
build_cmd = "cargo build"
test_cmd = "cargo test"
lint_cmd = "cargo clippy"
fmt_cmd = "cargo fmt --check"
"#;

    let config: ReposConfig = toml::from_str(toml_str).unwrap();
    let repo = &config.repo[0];

    // No [repo.ci] section → ci is None → CI disabled
    assert!(
        repo.ci.is_none(),
        "CI config should be None when not specified"
    );
    let ci_enabled = repo.ci.as_ref().is_some_and(|ci| ci.enabled);
    assert!(!ci_enabled, "CI should be disabled when no [ci] section");
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
