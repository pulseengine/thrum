//! File watcher for real-time change detection during agent tasks.
//!
//! Spawns a [`RecommendedWatcher`] with 500ms debounce on the repo working
//! directory. File-system events are mapped to [`EventKind::FileChanged`] and
//! emitted to the broadcast [`EventBus`]. A companion ticker runs
//! `git diff --stat` every 5 seconds and emits [`EventKind::DiffUpdate`]
//! events with aggregate change counts.
//!
//! Both background tasks are cancelled when the [`FileWatcher`] is stopped
//! (via [`FileWatcher::stop`] or when [`CancellationToken`] fires).

use crate::event_bus::EventBus;
use anyhow::{Context, Result};
use notify_debouncer_mini::{DebouncedEventKind, new_debouncer};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thrum_core::agent::AgentId;
use thrum_core::event::{EventKind, FileChangeKind};
use thrum_core::task::TaskId;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Debounce interval for file-system events.
const DEBOUNCE_MS: u64 = 500;

/// How often to run `git diff --stat`.
const DIFF_POLL_SECS: u64 = 5;

/// Watches a repository working directory for file changes during an agent task.
///
/// Created via [`FileWatcher::start`], stopped via [`FileWatcher::stop`].
/// Both the FS watcher and the diff poller are cancelled together.
pub struct FileWatcher {
    cancel: CancellationToken,
    fs_handle: JoinHandle<()>,
    diff_handle: JoinHandle<()>,
}

impl FileWatcher {
    /// Start watching `work_dir` for changes. Emits events tagged with
    /// the given `agent_id` and `task_id`.
    pub fn start(
        work_dir: PathBuf,
        agent_id: AgentId,
        task_id: TaskId,
        event_bus: EventBus,
    ) -> Result<Self> {
        let cancel = CancellationToken::new();

        let fs_handle = spawn_fs_watcher(
            work_dir.clone(),
            agent_id.clone(),
            task_id.clone(),
            event_bus.clone(),
            cancel.clone(),
        )?;

        let diff_handle = spawn_diff_poller(work_dir, agent_id, task_id, event_bus, cancel.clone());

        Ok(Self {
            cancel,
            fs_handle,
            diff_handle,
        })
    }

    /// Stop the watcher and wait for background tasks to finish.
    pub async fn stop(self) {
        self.cancel.cancel();
        // Best-effort join — if a task panicked, we log and continue.
        let _ = self.fs_handle.await;
        let _ = self.diff_handle.await;
    }
}

/// Spawn the debounced file-system watcher task.
///
/// Uses `notify-debouncer-mini` with a 500ms debounce window.  Events are
/// bridged from the synchronous notify callback to async via an mpsc channel.
fn spawn_fs_watcher(
    work_dir: PathBuf,
    agent_id: AgentId,
    task_id: TaskId,
    event_bus: EventBus,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>> {
    let (tx, rx) = mpsc::channel::<Vec<notify_debouncer_mini::DebouncedEvent>>(256);

    let mut debouncer = new_debouncer(
        Duration::from_millis(DEBOUNCE_MS),
        move |result: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
            if let Ok(events) = result {
                // Non-blocking: drop if the receiver is full or gone.
                let _ = tx.try_send(events);
            }
        },
    )
    .context("failed to create file watcher")?;

    debouncer
        .watcher()
        .watch(work_dir.as_ref(), notify::RecursiveMode::Recursive)
        .context("failed to watch working directory")?;

    let handle = tokio::spawn(async move {
        // Keep the debouncer alive for the duration of this task.
        let _debouncer = debouncer;
        let mut rx = rx;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                batch = rx.recv() => {
                    let Some(events) = batch else { break };
                    for event in events {
                        let path = event.path;

                        // Skip .git directory changes — they are internal bookkeeping.
                        if path_is_in_git_dir(&path) {
                            continue;
                        }

                        let kind = match event.kind {
                            DebouncedEventKind::Any => infer_change_kind(&path),
                            DebouncedEventKind::AnyContinuous => FileChangeKind::Modified,
                            _ => infer_change_kind(&path),
                        };

                        event_bus.emit(EventKind::FileChanged {
                            agent_id: agent_id.clone(),
                            task_id: task_id.clone(),
                            path,
                            kind,
                        });
                    }
                }
            }
        }

        tracing::debug!("file watcher stopped");
    });

    Ok(handle)
}

/// Spawn the periodic `git diff --stat` poller.
fn spawn_diff_poller(
    work_dir: PathBuf,
    agent_id: AgentId,
    task_id: TaskId,
    event_bus: EventBus,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(DIFF_POLL_SECS));
        // The first tick fires immediately — skip it so we don't emit a
        // (likely empty) diff update at agent start.
        interval.tick().await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    match run_git_diff_stat(&work_dir).await {
                        Ok(stats) => {
                            event_bus.emit(EventKind::DiffUpdate {
                                agent_id: agent_id.clone(),
                                task_id: task_id.clone(),
                                files_changed: stats.files_changed,
                                insertions: stats.insertions,
                                deletions: stats.deletions,
                            });
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "git diff --stat failed");
                        }
                    }
                }
            }
        }

        tracing::debug!("diff poller stopped");
    })
}

/// Parsed output of `git diff --stat`.
#[derive(Debug, Default)]
struct DiffStats {
    files_changed: u32,
    insertions: u32,
    deletions: u32,
}

/// Run `git diff --stat` and parse the summary line.
///
/// The summary line looks like:
///   `3 files changed, 42 insertions(+), 7 deletions(-)`
///
/// Any of the three parts may be absent (e.g. no deletions).
async fn run_git_diff_stat(work_dir: &Path) -> Result<DiffStats> {
    let output = tokio::process::Command::new("git")
        .args(["diff", "--stat"])
        .current_dir(work_dir)
        .output()
        .await
        .context("failed to run git diff --stat")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_diff_stat_summary(&stdout))
}

/// Parse the summary line from `git diff --stat` output.
fn parse_diff_stat_summary(output: &str) -> DiffStats {
    let mut stats = DiffStats::default();

    // The summary is the last non-empty line.
    let Some(summary) = output.lines().rev().find(|l| !l.trim().is_empty()) else {
        return stats;
    };

    // Match patterns like "3 files changed", "42 insertions(+)", "7 deletions(-)"
    for part in summary.split(',') {
        let part = part.trim();
        if part.contains("file") && part.contains("changed") {
            if let Some(n) = extract_leading_number(part) {
                stats.files_changed = n;
            }
        } else if part.contains("insertion")
            && let Some(n) = extract_leading_number(part)
        {
            stats.insertions = n;
        } else if part.contains("deletion")
            && let Some(n) = extract_leading_number(part)
        {
            stats.deletions = n;
        }
    }

    stats
}

/// Extract the first number from a string fragment like " 42 insertions(+)".
fn extract_leading_number(s: &str) -> Option<u32> {
    s.split_whitespace()
        .next()
        .and_then(|tok| tok.parse::<u32>().ok())
}

/// Check if a path is inside a `.git` directory.
fn path_is_in_git_dir(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".git")
}

/// Infer the change kind from whether the file exists on disk.
///
/// `notify-debouncer-mini` only reports `Any` / `AnyContinuous`, so we
/// probe the filesystem to distinguish created vs modified vs deleted.
fn infer_change_kind(path: &Path) -> FileChangeKind {
    if path.exists() {
        // Could be either Created or Modified — we can't distinguish perfectly
        // without tracking previous state.  We default to Modified because
        // it's the most common case during agent work; new files are less frequent.
        FileChangeKind::Modified
    } else {
        FileChangeKind::Deleted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_summary() {
        let output = " src/main.rs | 10 ++++++----\n src/lib.rs  |  5 ++---\n 2 files changed, 8 insertions(+), 7 deletions(-)\n";
        let stats = parse_diff_stat_summary(output);
        assert_eq!(stats.files_changed, 2);
        assert_eq!(stats.insertions, 8);
        assert_eq!(stats.deletions, 7);
    }

    #[test]
    fn parse_insertions_only() {
        let output = " 1 file changed, 42 insertions(+)\n";
        let stats = parse_diff_stat_summary(output);
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 42);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn parse_deletions_only() {
        let output = " 1 file changed, 3 deletions(-)\n";
        let stats = parse_diff_stat_summary(output);
        assert_eq!(stats.files_changed, 1);
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 3);
    }

    #[test]
    fn parse_empty_output() {
        let stats = parse_diff_stat_summary("");
        assert_eq!(stats.files_changed, 0);
        assert_eq!(stats.insertions, 0);
        assert_eq!(stats.deletions, 0);
    }

    #[test]
    fn git_dir_filtered() {
        assert!(path_is_in_git_dir(Path::new("/repo/.git/objects/pack/foo")));
        assert!(path_is_in_git_dir(Path::new(".git/HEAD")));
        assert!(!path_is_in_git_dir(Path::new("src/main.rs")));
        assert!(!path_is_in_git_dir(Path::new("docs/.gitkeep")));
    }

    #[test]
    fn infer_deleted_when_path_absent() {
        let kind = infer_change_kind(Path::new("/nonexistent/foo/bar.txt"));
        assert_eq!(kind, FileChangeKind::Deleted);
    }

    #[test]
    fn extract_number_from_summary_part() {
        assert_eq!(extract_leading_number("3 files changed"), Some(3));
        assert_eq!(extract_leading_number("42 insertions(+)"), Some(42));
        assert_eq!(extract_leading_number("no numbers here"), None);
    }
}
