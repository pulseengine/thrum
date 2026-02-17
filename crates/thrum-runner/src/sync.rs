//! Remote sync points with automatic rebase of in-flight work.
//!
//! After PRs are merged, the local `main` branch can fall behind the remote.
//! This module fetches remote changes, fast-forwards/rebases local main, and
//! rebases in-flight task branches onto the updated main.
//!
//! Sync is controlled by [`SyncStrategy`](thrum_core::repo::SyncStrategy):
//! - **Eager**: sync after every PR merge
//! - **Batched**: sync after N merges accumulate
//! - **Manual**: sync only via API/dashboard trigger

use crate::event_bus::EventBus;
use crate::git::GitRepo;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use thrum_core::event::EventKind;
use thrum_core::repo::{RepoConfig, SyncStrategy};
use thrum_core::task::{TaskId, TaskStatus};

/// Tracks the number of PR merges since the last sync.
///
/// Thread-safe via `AtomicU32` — multiple CI polling tasks can
/// call `record_merge()` concurrently without locking.
pub struct SyncTracker {
    counter: Arc<AtomicU32>,
}

impl SyncTracker {
    /// Create a new tracker starting at zero.
    pub fn new() -> Self {
        Self {
            counter: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Create a tracker from a shared atomic counter.
    pub fn from_shared(counter: Arc<AtomicU32>) -> Self {
        Self { counter }
    }

    /// Record that a PR was merged.
    pub fn record_merge(&self) {
        self.counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the number of merges since the last sync.
    pub fn pending_count(&self) -> u32 {
        self.counter.load(Ordering::Relaxed)
    }

    /// Reset the merge counter (called after a sync completes).
    pub fn reset(&self) {
        self.counter.store(0, Ordering::Relaxed);
    }

    /// Get the underlying shared counter for cross-task sharing.
    pub fn shared_counter(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.counter)
    }
}

impl Default for SyncTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a sync operation for a single repository.
#[derive(Debug)]
pub struct SyncResult {
    pub repo: String,
    pub main_updated: bool,
    pub old_sha: Option<String>,
    pub new_sha: Option<String>,
    pub rebased_branches: Vec<String>,
    pub conflict_branches: Vec<String>,
}

/// Determine whether a sync should be triggered based on strategy.
pub fn should_sync(strategy: &SyncStrategy, tracker: &SyncTracker, batch_size: u32) -> bool {
    match strategy {
        SyncStrategy::Eager => tracker.pending_count() > 0,
        SyncStrategy::Batched => tracker.pending_count() >= batch_size,
        SyncStrategy::Manual => false, // Only triggered explicitly
    }
}

/// Run a full sync cycle for a single repository.
///
/// 1. Fetch from remote
/// 2. Fast-forward/rebase local main
/// 3. Rebase all in-flight task branches onto updated main
/// 4. Emit events for each step
pub fn sync_repo(
    repo_config: &RepoConfig,
    in_flight_tasks: &[(TaskId, String)],
    event_bus: &EventBus,
    trigger: &str,
) -> anyhow::Result<SyncResult> {
    let repo_name = repo_config.name.to_string();
    let start = std::time::Instant::now();

    event_bus.emit(EventKind::SyncStarted {
        repo: repo_config.name.clone(),
        trigger: trigger.to_string(),
    });

    let git = GitRepo::open(&repo_config.path)?;

    // Step 1: Fetch from remote
    git.fetch_remote("origin")?;

    // Step 2: Sync local main to remote
    let (main_updated, old_sha, new_sha) = match git.sync_main_to_remote("origin")? {
        Some((old, new)) => {
            event_bus.emit(EventKind::SyncMainUpdated {
                repo: repo_config.name.clone(),
                old_sha: old.clone(),
                new_sha: new.clone(),
            });
            (true, Some(old), Some(new))
        }
        None => (false, None, None),
    };

    // Step 3: Rebase in-flight task branches
    let mut rebased_branches = Vec::new();
    let mut conflict_branches = Vec::new();

    if main_updated {
        let task_branches = git.list_task_branches().unwrap_or_default();

        for branch in &task_branches {
            // Find matching task ID for this branch
            let task_id = in_flight_tasks
                .iter()
                .find(|(_, b)| b == branch)
                .map(|(id, _)| id.clone());

            match git.rebase_branch_onto_main(branch) {
                Ok(true) => {
                    rebased_branches.push(branch.clone());
                    event_bus.emit(EventKind::SyncBranchRebased {
                        repo: repo_config.name.clone(),
                        branch: branch.clone(),
                        task_id,
                        success: true,
                        conflict_summary: None,
                    });
                }
                Ok(false) => {
                    conflict_branches.push(branch.clone());
                    event_bus.emit(EventKind::SyncBranchRebased {
                        repo: repo_config.name.clone(),
                        branch: branch.clone(),
                        task_id: task_id.clone(),
                        success: false,
                        conflict_summary: Some("rebase conflicts — aborted".into()),
                    });
                    // Dispatch rebase agent for conflict resolution
                    dispatch_rebase_agent(&repo_config.path, branch, task_id.as_ref(), event_bus);
                }
                Err(e) => {
                    tracing::warn!(
                        repo = %repo_name,
                        branch,
                        error = %e,
                        "failed to rebase branch"
                    );
                    conflict_branches.push(branch.clone());
                }
            }
        }
    }

    let duration_secs = start.elapsed().as_secs_f64();
    event_bus.emit(EventKind::SyncCompleted {
        repo: repo_config.name.clone(),
        branches_rebased: rebased_branches.len() as u32,
        conflicts: conflict_branches.len() as u32,
        duration_secs,
    });

    Ok(SyncResult {
        repo: repo_name,
        main_updated,
        old_sha,
        new_sha,
        rebased_branches,
        conflict_branches,
    })
}

/// Sync all repos that have CI enabled and need syncing.
pub fn sync_all_repos(
    repos: &[RepoConfig],
    trackers: &std::collections::HashMap<String, SyncTracker>,
    tasks: &[(TaskId, String, String)], // (task_id, branch, repo_name)
    event_bus: &EventBus,
    trigger: &str,
) -> Vec<SyncResult> {
    let mut results = Vec::new();

    for repo in repos {
        let ci = match &repo.ci {
            Some(ci) if ci.enabled => ci,
            _ => continue,
        };

        // Check if sync should trigger (skip for manual triggers — they always run)
        if trigger != "manual"
            && let Some(tracker) = trackers.get(&repo.name.to_string())
            && !should_sync(&ci.sync_strategy, tracker, ci.sync_batch_size)
        {
            continue;
        }

        // Collect in-flight tasks for this repo
        let repo_tasks: Vec<(TaskId, String)> = tasks
            .iter()
            .filter(|(_, _, r)| r == &repo.name.to_string())
            .map(|(id, branch, _)| (id.clone(), branch.clone()))
            .collect();

        match sync_repo(repo, &repo_tasks, event_bus, trigger) {
            Ok(result) => {
                // Reset the tracker after successful sync
                if let Some(tracker) = trackers.get(&repo.name.to_string()) {
                    tracker.reset();
                }
                results.push(result);
            }
            Err(e) => {
                tracing::error!(
                    repo = %repo.name,
                    error = %e,
                    "sync failed for repo"
                );
            }
        }
    }

    results
}

/// Placeholder for dispatching a rebase agent to resolve conflicts.
///
/// In the future, this will invoke an AI agent to resolve merge conflicts.
/// For now, it logs the conflict and emits an event.
fn dispatch_rebase_agent(
    _repo_path: &std::path::Path,
    branch: &str,
    task_id: Option<&TaskId>,
    _event_bus: &EventBus,
) {
    tracing::warn!(
        branch,
        task_id = ?task_id,
        "rebase conflict detected — manual resolution required (rebase agent not yet implemented)"
    );
}

/// Check if a task status represents an in-flight (actively worked on) task.
pub fn is_in_flight(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Implementing { .. }
            | TaskStatus::Reviewing { .. }
            | TaskStatus::AwaitingCI { .. }
            | TaskStatus::Integrating
            | TaskStatus::Claimed { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_records_and_resets() {
        let tracker = SyncTracker::new();
        assert_eq!(tracker.pending_count(), 0);

        tracker.record_merge();
        tracker.record_merge();
        assert_eq!(tracker.pending_count(), 2);

        tracker.reset();
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn tracker_shared_counter() {
        let tracker1 = SyncTracker::new();
        let counter = tracker1.shared_counter();
        let tracker2 = SyncTracker::from_shared(counter);

        tracker1.record_merge();
        assert_eq!(tracker2.pending_count(), 1);

        tracker2.record_merge();
        assert_eq!(tracker1.pending_count(), 2);
    }

    #[test]
    fn should_sync_eager() {
        let tracker = SyncTracker::new();
        assert!(!should_sync(&SyncStrategy::Eager, &tracker, 3));

        tracker.record_merge();
        assert!(should_sync(&SyncStrategy::Eager, &tracker, 3));
    }

    #[test]
    fn should_sync_batched() {
        let tracker = SyncTracker::new();
        tracker.record_merge();
        tracker.record_merge();
        assert!(!should_sync(&SyncStrategy::Batched, &tracker, 3));

        tracker.record_merge();
        assert!(should_sync(&SyncStrategy::Batched, &tracker, 3));
    }

    #[test]
    fn should_sync_manual_never() {
        let tracker = SyncTracker::new();
        tracker.record_merge();
        tracker.record_merge();
        tracker.record_merge();
        assert!(!should_sync(&SyncStrategy::Manual, &tracker, 1));
    }

    #[test]
    fn is_in_flight_matches_active_statuses() {
        assert!(is_in_flight(&TaskStatus::Implementing {
            branch: "auto/TASK-0001/repo/feat".into(),
            started_at: chrono::Utc::now(),
        }));
        assert!(is_in_flight(&TaskStatus::Integrating));
        assert!(!is_in_flight(&TaskStatus::Pending));
        assert!(!is_in_flight(&TaskStatus::Merged {
            commit_sha: "abc".into(),
        }));
    }

    #[test]
    fn default_tracker() {
        let tracker = SyncTracker::default();
        assert_eq!(tracker.pending_count(), 0);
    }
}
