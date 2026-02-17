//! CI status polling and failure recovery.
//!
//! Polls GitHub CI status via `gh pr checks` and handles pass/fail.
//! On CI failure, dispatches a ci_fixer agent to fix and re-push.
//! Tracks CI attempts and escalates to human review after max retries.

use crate::event_bus::EventBus;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use thrum_core::ci::{CICheck, CIPollResult, CIStatus};
use thrum_core::event::EventKind;
use thrum_core::task::{RepoName, Task, TaskId, TaskStatus};
use thrum_db::task_store::TaskStore;

/// Poll CI status for a PR using `gh pr checks`.
///
/// Returns the aggregated CI status and individual check results.
pub fn poll_ci_status(repo_path: &Path, pr_number: u64) -> Result<CIPollResult> {
    let output = Command::new("gh")
        .args([
            "pr",
            "checks",
            &pr_number.to_string(),
            "--json",
            "name,state,detailsUrl",
        ])
        .current_dir(repo_path)
        .output()
        .context("failed to run `gh pr checks`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // If no checks are configured, gh may fail
        if stderr.contains("no checks") || stderr.contains("no status checks") {
            return Ok(CIPollResult {
                status: CIStatus::NoChecks,
                checks: Vec::new(),
                summary: "No CI checks configured for this PR".into(),
            });
        }
        anyhow::bail!("gh pr checks failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let checks: Vec<GhCheck> =
        serde_json::from_str(&stdout).context("failed to parse gh pr checks output")?;

    if checks.is_empty() {
        return Ok(CIPollResult {
            status: CIStatus::NoChecks,
            checks: Vec::new(),
            summary: "No CI checks found".into(),
        });
    }

    let ci_checks: Vec<CICheck> = checks
        .iter()
        .map(|c| CICheck {
            name: c.name.clone(),
            status: c.state.to_lowercase(),
            url: c.details_url.clone(),
        })
        .collect();

    let any_pending = ci_checks.iter().any(|c| {
        c.status == "pending"
            || c.status == "queued"
            || c.status == "in_progress"
            || c.status == "waiting"
    });
    let any_failed = ci_checks
        .iter()
        .any(|c| c.status == "failure" || c.status == "error" || c.status == "cancelled");

    let status = if any_pending {
        CIStatus::Pending
    } else if any_failed {
        CIStatus::Fail
    } else {
        CIStatus::Pass
    };

    let passed = ci_checks.iter().filter(|c| c.status == "success").count();
    let failed = ci_checks
        .iter()
        .filter(|c| c.status == "failure" || c.status == "error")
        .count();
    let pending = ci_checks.len() - passed - failed;

    let summary = format!(
        "{passed} passed, {failed} failed, {pending} pending (total: {})",
        ci_checks.len()
    );

    Ok(CIPollResult {
        status,
        checks: ci_checks,
        summary,
    })
}

/// Merge a PR via `gh pr merge`.
pub fn merge_pr(repo_path: &Path, pr_number: u64, strategy: &str) -> Result<String> {
    let strategy_flag = match strategy {
        "squash" => "--squash",
        "rebase" => "--rebase",
        "merge" => "--merge",
        _ => "--squash",
    };

    let output = Command::new("gh")
        .args([
            "pr",
            "merge",
            &pr_number.to_string(),
            strategy_flag,
            "--delete-branch",
        ])
        .current_dir(repo_path)
        .output()
        .context("failed to run `gh pr merge`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh pr merge failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout)
}

/// Get the merge commit SHA after a PR merge.
pub fn get_pr_merge_sha(repo_path: &Path, pr_number: u64) -> Result<String> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "mergeCommit",
            "-q",
            ".mergeCommit.oid",
        ])
        .current_dir(repo_path)
        .output()
        .context("failed to get merge commit SHA")?;

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        // Fallback: get the HEAD sha from the default branch
        let head_output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_path)
            .output()
            .context("failed to get HEAD sha")?;
        Ok(String::from_utf8_lossy(&head_output.stdout)
            .trim()
            .to_string())
    } else {
        Ok(sha)
    }
}

/// Get CI failure logs via `gh run view --log-failed`.
pub fn get_ci_failure_logs(repo_path: &Path, pr_number: u64) -> Result<String> {
    // First, get the failed run IDs from the PR checks
    let output = Command::new("gh")
        .args([
            "pr",
            "checks",
            &pr_number.to_string(),
            "--json",
            "name,state,detailsUrl",
        ])
        .current_dir(repo_path)
        .output()
        .context("failed to get PR checks")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let checks: Vec<GhCheck> = serde_json::from_str(&stdout).unwrap_or_default();

    let failed_checks: Vec<&GhCheck> = checks
        .iter()
        .filter(|c| {
            let s = c.state.to_lowercase();
            s == "failure" || s == "error"
        })
        .collect();

    if failed_checks.is_empty() {
        return Ok("No failed checks found.".into());
    }

    // Build a summary of failed checks
    let mut logs = String::new();
    logs.push_str(&format!(
        "## CI Failure Summary ({} failed check(s))\n\n",
        failed_checks.len()
    ));

    for check in &failed_checks {
        logs.push_str(&format!("### {} ({})\n", check.name, check.state));
        if let Some(url) = &check.details_url {
            logs.push_str(&format!("URL: {url}\n"));
        }
        logs.push('\n');
    }

    // Try to get detailed logs from the most recent failed run
    let run_output = Command::new("gh")
        .args([
            "run",
            "list",
            "--branch",
            "--json",
            "databaseId,status,conclusion",
            "--limit",
            "1",
        ])
        .current_dir(repo_path)
        .output();

    if let Ok(run_out) = run_output
        && run_out.status.success()
    {
        let run_stdout = String::from_utf8_lossy(&run_out.stdout);
        let runs: Vec<serde_json::Value> = serde_json::from_str(&run_stdout).unwrap_or_default();

        if let Some(run) = runs.first()
            && let Some(run_id) = run.get("databaseId").and_then(|v| v.as_u64())
        {
            let log_output = Command::new("gh")
                .args(["run", "view", &run_id.to_string(), "--log-failed"])
                .current_dir(repo_path)
                .output();

            if let Ok(log_out) = log_output
                && log_out.status.success()
            {
                let log_text = String::from_utf8_lossy(&log_out.stdout);
                // Truncate to a reasonable size for the agent
                let truncated: String = log_text.chars().take(10000).collect();
                logs.push_str("## Failed Run Logs\n\n```\n");
                logs.push_str(&truncated);
                if log_text.len() > 10000 {
                    logs.push_str("\n... (truncated)");
                }
                logs.push_str("\n```\n");
            }
        }
    }

    Ok(logs)
}

/// Push a branch to the remote.
pub fn push_branch(repo_path: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["push", "-u", "origin", branch])
        .current_dir(repo_path)
        .output()
        .context("failed to push branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Force push if the branch already exists with different history
        if stderr.contains("rejected") || stderr.contains("non-fast-forward") {
            let force_output = Command::new("git")
                .args(["push", "--force-with-lease", "-u", "origin", branch])
                .current_dir(repo_path)
                .output()
                .context("failed to force-push branch")?;

            if !force_output.status.success() {
                let stderr2 = String::from_utf8_lossy(&force_output.stderr);
                anyhow::bail!("git push failed: {stderr2}");
            }
        } else {
            anyhow::bail!("git push failed: {stderr}");
        }
    }

    Ok(())
}

/// Create a PR via `gh pr create`.
///
/// Returns (pr_number, pr_url).
pub fn create_pr(repo_path: &Path, branch: &str, title: &str, body: &str) -> Result<(u64, String)> {
    let output = Command::new("gh")
        .args([
            "pr",
            "create",
            "--head",
            branch,
            "--title",
            title,
            "--body",
            body,
            "--json",
            "number,url",
        ])
        .current_dir(repo_path)
        .output()
        .context("failed to run `gh pr create`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Check if PR already exists
        if stderr.contains("already exists") {
            // Get existing PR info
            return get_existing_pr(repo_path, branch);
        }
        anyhow::bail!("gh pr create failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pr: serde_json::Value =
        serde_json::from_str(&stdout).context("failed to parse gh pr create output")?;

    let pr_number = pr
        .get("number")
        .and_then(|v| v.as_u64())
        .context("missing PR number in response")?;
    let pr_url = pr
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((pr_number, pr_url))
}

/// Get an existing PR for a branch.
fn get_existing_pr(repo_path: &Path, branch: &str) -> Result<(u64, String)> {
    let output = Command::new("gh")
        .args(["pr", "view", branch, "--json", "number,url"])
        .current_dir(repo_path)
        .output()
        .context("failed to get existing PR")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to find existing PR for branch {branch}: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pr: serde_json::Value = serde_json::from_str(&stdout).context("failed to parse PR info")?;

    let pr_number = pr
        .get("number")
        .and_then(|v| v.as_u64())
        .context("missing PR number")?;
    let pr_url = pr
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((pr_number, pr_url))
}

/// Poll CI status in a loop until pass/fail/timeout.
///
/// Returns the final CI status. Emits events to the event bus
/// during polling for real-time dashboard updates.
pub async fn poll_ci_until_complete(
    repo_path: &Path,
    task_id: &TaskId,
    repo: &RepoName,
    pr_number: u64,
    poll_interval: Duration,
    event_bus: &EventBus,
) -> Result<CIPollResult> {
    // Maximum total polling time: 1 hour
    let max_polls = 3600 / poll_interval.as_secs().max(1);
    let mut poll_count = 0u64;

    loop {
        poll_count += 1;
        if poll_count > max_polls {
            return Ok(CIPollResult {
                status: CIStatus::Fail,
                checks: Vec::new(),
                summary: "CI polling timed out after 1 hour".into(),
            });
        }

        let result = poll_ci_status(repo_path, pr_number)?;

        event_bus.emit(EventKind::CICheckUpdate {
            task_id: task_id.clone(),
            repo: repo.clone(),
            pr_number,
            status: result.status.to_string(),
            summary: result.summary.clone(),
        });

        match result.status {
            CIStatus::Pending => {
                tracing::debug!(
                    task_id = %task_id,
                    pr_number,
                    poll = poll_count,
                    summary = %result.summary,
                    "CI still pending, waiting..."
                );
                tokio::time::sleep(poll_interval).await;
            }
            CIStatus::Pass | CIStatus::Fail | CIStatus::NoChecks => {
                return Ok(result);
            }
        }
    }
}

/// Run the CI polling and fix loop for a task in AwaitingCI status.
///
/// This is the main entry point called by the parallel engine.
/// It polls CI, handles pass/fail, dispatches ci_fixer on failure,
/// and escalates after max retries.
#[allow(clippy::too_many_arguments)]
pub async fn run_ci_loop(
    task_store: &TaskStore<'_>,
    event_bus: &EventBus,
    repo_path: &Path,
    agents_dir: &Path,
    registry: &crate::backend::BackendRegistry,
    roles: Option<&thrum_core::role::RolesConfig>,
    worktrees_dir: &Path,
    mut task: Task,
    sync_tracker: Option<&crate::sync::SyncTracker>,
) -> Result<()> {
    let (
        pr_number,
        pr_url,
        branch,
        ci_attempts,
        max_retries,
        poll_interval,
        auto_merge,
        merge_strategy,
    ) = match &task.status {
        TaskStatus::AwaitingCI {
            pr_number,
            pr_url,
            branch,
            ci_attempts,
            ..
        } => {
            // Get CI config from context or use defaults
            let ci_config = thrum_core::repo::CIConfig::default();
            (
                *pr_number,
                pr_url.clone(),
                branch.clone(),
                *ci_attempts,
                ci_config.max_ci_retries,
                Duration::from_secs(ci_config.poll_interval_secs),
                ci_config.auto_merge,
                ci_config.merge_strategy.clone(),
            )
        }
        _ => {
            tracing::warn!(
                task_id = %task.id,
                status = task.status.label(),
                "run_ci_loop called on non-AwaitingCI task"
            );
            return Ok(());
        }
    };

    tracing::info!(
        task_id = %task.id,
        pr_number,
        pr_url = %pr_url,
        ci_attempts,
        "starting CI polling loop"
    );

    event_bus.emit(EventKind::CIPollingStarted {
        task_id: task.id.clone(),
        repo: task.repo.clone(),
        pr_number,
        pr_url: pr_url.clone(),
    });

    // Poll CI status
    let result = poll_ci_until_complete(
        repo_path,
        &task.id,
        &task.repo,
        pr_number,
        poll_interval,
        event_bus,
    )
    .await?;

    match result.status {
        CIStatus::Pass | CIStatus::NoChecks => {
            // CI passed — merge the PR
            event_bus.emit(EventKind::CIPassed {
                task_id: task.id.clone(),
                repo: task.repo.clone(),
                pr_number,
            });

            if auto_merge {
                tracing::info!(
                    task_id = %task.id,
                    pr_number,
                    strategy = %merge_strategy,
                    "CI passed, merging PR"
                );
                merge_pr(repo_path, pr_number, &merge_strategy)?;

                let commit_sha =
                    get_pr_merge_sha(repo_path, pr_number).unwrap_or_else(|_| "pr-merged".into());

                let old_label = task.status.label().to_string();
                task.status = TaskStatus::Merged { commit_sha };
                task.updated_at = chrono::Utc::now();
                task_store.update(&task)?;

                event_bus.emit(EventKind::TaskStateChange {
                    task_id: task.id.clone(),
                    repo: task.repo.clone(),
                    from: old_label,
                    to: "merged".into(),
                });

                // Record the merge for sync tracking.
                if let Some(tracker) = sync_tracker {
                    tracker.record_merge();
                }

                tracing::info!(task_id = %task.id, "task merged via CI");
            } else {
                tracing::info!(
                    task_id = %task.id,
                    "CI passed but auto_merge disabled — task stays in awaiting-ci"
                );
            }
        }
        CIStatus::Fail => {
            let current_attempt = ci_attempts + 1;

            event_bus.emit(EventKind::CIFailed {
                task_id: task.id.clone(),
                repo: task.repo.clone(),
                pr_number,
                attempt: current_attempt,
                max_attempts: max_retries,
                failure_summary: result.summary.clone(),
            });

            if current_attempt > max_retries {
                // Escalate to human review
                tracing::warn!(
                    task_id = %task.id,
                    attempts = current_attempt,
                    max_retries,
                    "CI retries exhausted, escalating to human review"
                );

                event_bus.emit(EventKind::CIEscalated {
                    task_id: task.id.clone(),
                    repo: task.repo.clone(),
                    pr_number,
                    attempts: current_attempt,
                    failure_summary: result.summary.clone(),
                });

                let old_label = task.status.label().to_string();
                task.status = TaskStatus::CIFailed {
                    pr_number,
                    pr_url,
                    failure_summary: result.summary,
                    ci_attempts: current_attempt,
                };
                task.updated_at = chrono::Utc::now();
                task_store.update(&task)?;

                event_bus.emit(EventKind::TaskStateChange {
                    task_id: task.id.clone(),
                    repo: task.repo.clone(),
                    from: old_label,
                    to: "ci-failed".into(),
                });
            } else {
                // Dispatch ci_fixer agent
                tracing::info!(
                    task_id = %task.id,
                    attempt = current_attempt,
                    max_retries,
                    "dispatching ci_fixer agent"
                );

                dispatch_ci_fixer(
                    task_store,
                    event_bus,
                    repo_path,
                    agents_dir,
                    registry,
                    roles,
                    worktrees_dir,
                    &mut task,
                    pr_number,
                    &pr_url,
                    &branch,
                    current_attempt,
                    max_retries,
                )
                .await?;
            }
        }
        CIStatus::Pending => {
            // Should not happen — poll_ci_until_complete loops until non-pending
            tracing::warn!(task_id = %task.id, "CI polling returned Pending unexpectedly");
        }
    }

    Ok(())
}

/// Dispatch the ci_fixer agent to fix CI failures and re-push.
#[allow(clippy::too_many_arguments)]
async fn dispatch_ci_fixer(
    task_store: &TaskStore<'_>,
    event_bus: &EventBus,
    repo_path: &Path,
    agents_dir: &Path,
    registry: &crate::backend::BackendRegistry,
    roles: Option<&thrum_core::role::RolesConfig>,
    _worktrees_dir: &Path,
    task: &mut Task,
    pr_number: u64,
    pr_url: &str,
    branch: &str,
    current_attempt: u32,
    max_retries: u32,
) -> Result<()> {
    // Get CI failure logs
    let failure_logs = get_ci_failure_logs(repo_path, pr_number)
        .unwrap_or_else(|e| format!("Failed to get CI logs: {e}"));

    // Load the ci_fixer prompt template
    let ci_fixer_prompt_file = agents_dir.join("ci_fixer.md");
    let system_prompt = crate::claude::load_agent_prompt(&ci_fixer_prompt_file, None)
        .await
        .unwrap_or_else(|_| default_ci_fixer_prompt());

    // Build the prompt
    let prompt = format!(
        "## CI Fix Required\n\n\
         **Task**: {} ({})\n\
         **PR**: #{pr_number} ({pr_url})\n\
         **Branch**: {branch}\n\
         **Attempt**: {current_attempt}/{max_retries}\n\n\
         ## CI Failure Logs\n\n{failure_logs}\n\n\
         ## Instructions\n\n\
         1. Read the CI failure logs above carefully\n\
         2. Identify the root cause of the failure\n\
         3. Fix the issue in the codebase\n\
         4. Run the relevant tests locally to verify your fix\n\
         5. Commit and push your changes\n\n\
         The fix should be minimal and targeted — only change what's needed to make CI pass.\n\
         Do NOT refactor or add features. Focus solely on fixing the CI failure.",
        task.id, task.title
    );

    // Resolve the ci_fixer backend
    let (agent, _role_budget) = if let Some(roles) = roles {
        let role = roles.ci_fixer();
        let backend = registry
            .resolve_role(&role)
            .or_else(|| registry.agent())
            .context("no backend available for ci_fixer role")?;
        let budget = role.budget_usd.unwrap_or(3.0);
        (backend, budget)
    } else {
        let backend = registry.agent().context("no agent backend available")?;
        (backend, 3.0)
    };

    tracing::info!(
        task_id = %task.id,
        backend = agent.name(),
        "invoking ci_fixer agent"
    );

    // Invoke the ci_fixer agent — it works on the repo directly
    // (the branch should already be checked out or available)
    let request = crate::backend::AiRequest::new(&prompt)
        .with_system(system_prompt)
        .with_cwd(repo_path.to_path_buf());

    let result = agent.invoke(&request).await?;

    if result.exit_code.is_some_and(|c| c != 0) && !result.timed_out {
        tracing::warn!(
            task_id = %task.id,
            exit_code = ?result.exit_code,
            "ci_fixer agent failed"
        );
    }

    // Push the fix (the agent should have committed changes)
    match push_branch(repo_path, branch) {
        Ok(()) => {
            tracing::info!(
                task_id = %task.id,
                branch,
                "ci_fixer pushed fix commit"
            );

            event_bus.emit(EventKind::CIFixPushed {
                task_id: task.id.clone(),
                repo: task.repo.clone(),
                pr_number,
                attempt: current_attempt,
            });

            // Update task with incremented CI attempts, back to AwaitingCI
            let old_label = task.status.label().to_string();
            task.status = TaskStatus::AwaitingCI {
                pr_number,
                pr_url: pr_url.to_string(),
                branch: branch.to_string(),
                started_at: chrono::Utc::now(),
                ci_attempts: current_attempt,
            };
            task.updated_at = chrono::Utc::now();
            task_store.update(task)?;

            event_bus.emit(EventKind::TaskStateChange {
                task_id: task.id.clone(),
                repo: task.repo.clone(),
                from: old_label,
                to: "awaiting-ci".into(),
            });
        }
        Err(e) => {
            tracing::error!(
                task_id = %task.id,
                error = %e,
                "failed to push ci_fixer changes"
            );
            // Escalate since we can't push
            let old_label = task.status.label().to_string();
            task.status = TaskStatus::CIFailed {
                pr_number,
                pr_url: pr_url.to_string(),
                failure_summary: format!("ci_fixer push failed: {e}"),
                ci_attempts: current_attempt,
            };
            task.updated_at = chrono::Utc::now();
            task_store.update(task)?;

            event_bus.emit(EventKind::TaskStateChange {
                task_id: task.id.clone(),
                repo: task.repo.clone(),
                from: old_label,
                to: "ci-failed".into(),
            });
        }
    }

    Ok(())
}

/// Default ci_fixer system prompt when no template file exists.
fn default_ci_fixer_prompt() -> String {
    "You are a CI Fix Agent. Your sole job is to fix CI failures on a pull request branch.\n\n\
     ## Process\n\
     1. Read the CI failure logs provided in the prompt\n\
     2. Identify the root cause (build error, test failure, lint issue, etc.)\n\
     3. Make the minimum necessary fix\n\
     4. Run relevant checks locally to verify\n\
     5. Commit the fix with a clear message like \"fix: resolve CI failure in <component>\"\n\n\
     ## Rules\n\
     - Make MINIMAL changes — only fix the CI failure\n\
     - Do NOT refactor, add features, or restructure code\n\
     - Do NOT modify CI configuration unless the config itself is the bug\n\
     - Commit your fix before exiting\n"
        .into()
}

/// JSON structure returned by `gh pr checks --json`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhCheck {
    name: String,
    state: String,
    #[serde(default)]
    details_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_status_display() {
        assert_eq!(CIStatus::Pending.to_string(), "pending");
        assert_eq!(CIStatus::Pass.to_string(), "pass");
        assert_eq!(CIStatus::Fail.to_string(), "fail");
        assert_eq!(CIStatus::NoChecks.to_string(), "no-checks");
    }

    #[test]
    fn default_ci_fixer_prompt_not_empty() {
        let prompt = default_ci_fixer_prompt();
        assert!(!prompt.is_empty());
        assert!(prompt.contains("CI Fix Agent"));
    }

    #[test]
    fn ci_config_defaults() {
        let config = thrum_core::repo::CIConfig::default();
        assert!(config.enabled);
        assert_eq!(config.poll_interval_secs, 60);
        assert_eq!(config.max_ci_retries, 3);
        assert!(config.auto_merge);
        assert_eq!(config.merge_strategy, "squash");
    }

    #[test]
    fn task_status_awaiting_ci() {
        let status = TaskStatus::AwaitingCI {
            pr_number: 42,
            pr_url: "https://github.com/org/repo/pull/42".into(),
            branch: "auto/TASK-0001/repo/feature".into(),
            started_at: chrono::Utc::now(),
            ci_attempts: 0,
        };
        assert_eq!(status.label(), "awaiting-ci");
        assert!(status.is_awaiting_ci());
        assert!(!status.is_terminal());
        assert!(!status.needs_human());
    }

    #[test]
    fn task_status_ci_failed() {
        let status = TaskStatus::CIFailed {
            pr_number: 42,
            pr_url: "https://github.com/org/repo/pull/42".into(),
            failure_summary: "test failure".into(),
            ci_attempts: 3,
        };
        assert_eq!(status.label(), "ci-failed");
        assert!(status.needs_human());
        assert!(!status.is_terminal());
    }
}
